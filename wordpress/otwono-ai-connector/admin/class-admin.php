<?php
/**
 * The administration screens: setup, connection, diagnostics.
 *
 * Every form checks a nonce and a capability. Nothing on these screens is
 * reachable by a member.
 *
 * @package OTWONO\Connector
 */

declare( strict_types = 1 );

namespace OTWONO\Connector;

defined( 'ABSPATH' ) || exit;

final class Admin {

	private const SLUG  = 'otwono-connector';
	private const NONCE = 'otwono_connector_settings';

	public static function hooks(): void {
		add_action( 'admin_menu', array( self::class, 'menu' ) );
		add_action( 'admin_post_otwono_save_settings', array( self::class, 'handle_save' ) );
		add_action( 'admin_post_otwono_pair', array( self::class, 'handle_pair' ) );
		add_action( 'admin_post_otwono_unpair', array( self::class, 'handle_unpair' ) );
		add_filter(
			'plugin_action_links_' . plugin_basename( OTWONO_CONNECTOR_FILE ),
			array( self::class, 'action_links' )
		);
	}

	public static function action_links( array $links ): array {
		array_unshift(
			$links,
			sprintf(
				'<a href="%s">%s</a>',
				esc_url( admin_url( 'options-general.php?page=' . self::SLUG ) ),
				esc_html__( 'Settings', 'otwono-ai-connector' )
			)
		);
		return $links;
	}

	public static function menu(): void {
		add_options_page(
			__( 'OTWONO AI', 'otwono-ai-connector' ),
			__( 'OTWONO AI', 'otwono-ai-connector' ),
			'manage_options',
			self::SLUG,
			array( self::class, 'render' )
		);
	}

	private static function guard(): void {
		if ( ! current_user_can( 'manage_options' ) ) {
			wp_die(
				esc_html__( 'You are not allowed to change these settings.', 'otwono-ai-connector' ),
				'',
				array( 'response' => 403 )
			);
		}
		check_admin_referer( self::NONCE );
	}

	private static function redirect( string $message, string $tone = 'success' ): void {
		wp_safe_redirect(
			add_query_arg(
				array(
					'page'           => self::SLUG,
					'otwono_message' => rawurlencode( $message ),
					'otwono_tone'    => $tone,
				),
				admin_url( 'options-general.php' )
			)
		);
		exit;
	}

	public static function handle_save(): void {
		self::guard();

		$input = array(
			'mode'      => sanitize_text_field( wp_unslash( (string) ( $_POST['mode'] ?? 'relay' ) ) ),
			'relay_url' => sanitize_text_field( wp_unslash( (string) ( $_POST['relay_url'] ?? '' ) ) ),
			'local_url' => sanitize_text_field( wp_unslash( (string) ( $_POST['local_url'] ?? '' ) ) ),
			'allow_registration'       => isset( $_POST['allow_registration'] ),
			'delete_data_on_uninstall' => isset( $_POST['delete_data_on_uninstall'] ),
		);

		$saved = Settings::save( array_merge( Settings::all(), $input ) );
		Logger::record( 'settings_saved', array( 'mode' => $saved['mode'] ) );

		if ( 'relay' === $saved['mode'] && '' === $saved['relay_url'] ) {
			self::redirect(
				__( 'Saved, but the relay address was refused. It must be an https address that is not a private or local host.', 'otwono-ai-connector' ),
				'warning'
			);
		}
		self::redirect( __( 'Settings saved.', 'otwono-ai-connector' ) );
	}

	public static function handle_pair(): void {
		self::guard();

		$code = sanitize_text_field( wp_unslash( (string) ( $_POST['pairing_code'] ?? '' ) ) );
		if ( '' === $code ) {
			self::redirect( __( 'Enter the code shown in the OTWONO desktop app.', 'otwono-ai-connector' ), 'warning' );
		}

		$result = Client::post(
			'/v1/pairings/redeem',
			array( 'code' => $code, 'site' => home_url() )
		);

		if ( is_wp_error( $result ) ) {
			Logger::record( 'pairing_failed', array(), 'denied' );
			self::redirect( $result->get_error_message(), 'error' );
		}

		Settings::set_token( (string) ( $result['token'] ?? '' ) );
		Settings::save(
			array_merge(
				Settings::all(),
				array(
					'account_id' => (string) ( $result['account_id'] ?? '' ),
					'scopes'     => is_array( $result['scopes'] ?? null ) ? $result['scopes'] : array(),
					'paired_at'  => gmdate( 'c' ),
				)
			)
		);
		Logger::record( 'pairing_succeeded', array( 'scopes' => $result['scopes'] ?? array() ) );

		self::redirect( __( 'This site is now paired with your OTWONO account.', 'otwono-ai-connector' ) );
	}

	public static function handle_unpair(): void {
		self::guard();
		Settings::set_token( '' );
		Settings::save( array_merge( Settings::all(), array( 'account_id' => '', 'scopes' => array(), 'paired_at' => '' ) ) );
		Logger::record( 'unpaired', array() );
		self::redirect( __( 'This site is no longer paired.', 'otwono-ai-connector' ) );
	}

	public static function render(): void {
		if ( ! current_user_can( 'manage_options' ) ) {
			return;
		}

		$settings = Settings::all();
		$health   = Settings::is_configured() ? Client::health() : null;

		$message = isset( $_GET['otwono_message'] )
			? sanitize_text_field( wp_unslash( (string) $_GET['otwono_message'] ) )
			: '';
		$tone = isset( $_GET['otwono_tone'] )
			? sanitize_key( wp_unslash( (string) $_GET['otwono_tone'] ) )
			: 'success';
		?>
		<div class="wrap">
			<h1><?php esc_html_e( 'OTWONO AI Connector', 'otwono-ai-connector' ); ?></h1>

			<?php if ( '' !== $message ) : ?>
				<div class="notice notice-<?php echo esc_attr( in_array( $tone, array( 'success', 'warning', 'error' ), true ) ? $tone : 'info' ); ?> is-dismissible">
					<p><?php echo esc_html( $message ); ?></p>
				</div>
			<?php endif; ?>

			<h2><?php esc_html_e( 'Setup', 'otwono-ai-connector' ); ?></h2>
			<p>
				<?php esc_html_e(
					'This plugin connects your site to OTWONO AI so members can sign in, keep a profile, and see the projects they chose to synchronise. It never receives their conversations, files or knowledge.',
					'otwono-ai-connector'
				); ?>
			</p>

			<form method="post" action="<?php echo esc_url( admin_url( 'admin-post.php' ) ); ?>">
				<input type="hidden" name="action" value="otwono_save_settings">
				<?php wp_nonce_field( self::NONCE ); ?>

				<table class="form-table" role="presentation">
					<tr>
						<th scope="row"><label for="otwono-mode"><?php esc_html_e( 'Mode', 'otwono-ai-connector' ); ?></label></th>
						<td>
							<select name="mode" id="otwono-mode">
								<option value="relay" <?php selected( $settings['mode'], 'relay' ); ?>>
									<?php esc_html_e( 'Hosted relay (recommended)', 'otwono-ai-connector' ); ?>
								</option>
								<option value="local" <?php selected( $settings['mode'], 'local' ); ?>>
									<?php esc_html_e( 'Local development', 'otwono-ai-connector' ); ?>
								</option>
							</select>
							<p class="description">
								<?php esc_html_e(
									'Hosted relay is the only mode suitable for a public site. Local development is for a site running on the same machine as OTWONO.',
									'otwono-ai-connector'
								); ?>
							</p>
						</td>
					</tr>
					<tr>
						<th scope="row"><label for="otwono-relay-url"><?php esc_html_e( 'Relay address', 'otwono-ai-connector' ); ?></label></th>
						<td>
							<input type="url" class="regular-text" id="otwono-relay-url" name="relay_url"
								value="<?php echo esc_attr( $settings['relay_url'] ); ?>"
								placeholder="https://relay.example.com">
							<p class="description">
								<?php esc_html_e(
									'Must be https, and must not be a private or local address.',
									'otwono-ai-connector'
								); ?>
							</p>
						</td>
					</tr>
					<tr>
						<th scope="row"><label for="otwono-local-url"><?php esc_html_e( 'Local address', 'otwono-ai-connector' ); ?></label></th>
						<td>
							<input type="url" class="regular-text" id="otwono-local-url" name="local_url"
								value="<?php echo esc_attr( $settings['local_url'] ); ?>">
							<p class="description">
								<?php esc_html_e( 'Used only in local development mode.', 'otwono-ai-connector' ); ?>
							</p>
						</td>
					</tr>
					<tr>
						<th scope="row"><?php esc_html_e( 'Members', 'otwono-ai-connector' ); ?></th>
						<td>
							<label>
								<input type="checkbox" name="allow_registration"
									<?php checked( ! empty( $settings['allow_registration'] ) ); ?>>
								<?php esc_html_e( 'Let members create an OTWONO account from this site', 'otwono-ai-connector' ); ?>
							</label>
						</td>
					</tr>
					<tr>
						<th scope="row"><?php esc_html_e( 'Uninstalling', 'otwono-ai-connector' ); ?></th>
						<td>
							<label>
								<input type="checkbox" name="delete_data_on_uninstall"
									<?php checked( ! empty( $settings['delete_data_on_uninstall'] ) ); ?>>
								<?php esc_html_e( 'Delete members\' OTWONO links when this plugin is deleted', 'otwono-ai-connector' ); ?>
							</label>
							<p class="description">
								<?php esc_html_e(
									'Off by default. With this off, deleting the plugin removes its settings but leaves every member\'s data alone.',
									'otwono-ai-connector'
								); ?>
							</p>
						</td>
					</tr>
				</table>

				<?php submit_button( __( 'Save settings', 'otwono-ai-connector' ) ); ?>
			</form>

			<hr>

			<h2><?php esc_html_e( 'Connection', 'otwono-ai-connector' ); ?></h2>
			<?php if ( Settings::has_token() ) : ?>
				<p>
					<?php
					printf(
						/* translators: %s: the OTWONO account identifier. */
						esc_html__( 'Paired with account %s.', 'otwono-ai-connector' ),
						'<code>' . esc_html( (string) $settings['account_id'] ) . '</code>'
					);
					?>
				</p>
				<p>
					<?php esc_html_e( 'Permissions:', 'otwono-ai-connector' ); ?>
					<code><?php echo esc_html( implode( ', ', (array) $settings['scopes'] ) ); ?></code>
				</p>
				<form method="post" action="<?php echo esc_url( admin_url( 'admin-post.php' ) ); ?>">
					<input type="hidden" name="action" value="otwono_unpair">
					<?php wp_nonce_field( self::NONCE ); ?>
					<?php submit_button( __( 'Unpair this site', 'otwono-ai-connector' ), 'delete', 'submit', false ); ?>
				</form>
			<?php else : ?>
				<p>
					<?php esc_html_e(
						'In the OTWONO desktop application, open Settings and choose "Show a pairing code". Enter it here within five minutes.',
						'otwono-ai-connector'
					); ?>
				</p>
				<form method="post" action="<?php echo esc_url( admin_url( 'admin-post.php' ) ); ?>">
					<input type="hidden" name="action" value="otwono_pair">
					<?php wp_nonce_field( self::NONCE ); ?>
					<p>
						<label for="otwono-code" class="screen-reader-text">
							<?php esc_html_e( 'Pairing code', 'otwono-ai-connector' ); ?>
						</label>
						<input type="text" id="otwono-code" name="pairing_code" class="regular-text"
							autocomplete="off" placeholder="ABCD2345" maxlength="16">
					</p>
					<?php submit_button( __( 'Pair this site', 'otwono-ai-connector' ), 'primary', 'submit', false ); ?>
				</form>
			<?php endif; ?>

			<hr>

			<h2><?php esc_html_e( 'Diagnostics', 'otwono-ai-connector' ); ?></h2>
			<table class="widefat striped">
				<tbody>
					<tr>
						<th scope="row"><?php esc_html_e( 'Plugin version', 'otwono-ai-connector' ); ?></th>
						<td><?php echo esc_html( VERSION ); ?></td>
					</tr>
					<tr>
						<th scope="row"><?php esc_html_e( 'Address in use', 'otwono-ai-connector' ); ?></th>
						<td><code><?php echo esc_html( Settings::base_url() ?: '—' ); ?></code></td>
					</tr>
					<tr>
						<th scope="row"><?php esc_html_e( 'Reachable', 'otwono-ai-connector' ); ?></th>
						<td>
							<?php
							if ( null === $health ) {
								esc_html_e( 'Not configured yet.', 'otwono-ai-connector' );
							} elseif ( is_wp_error( $health ) ) {
								echo esc_html( $health->get_error_message() );
							} else {
								echo esc_html( (string) ( $health['stores'] ?? __( 'Yes.', 'otwono-ai-connector' ) ) );
							}
							?>
						</td>
					</tr>
					<tr>
						<th scope="row"><?php esc_html_e( 'PHP', 'otwono-ai-connector' ); ?></th>
						<td><?php echo esc_html( PHP_VERSION ); ?></td>
					</tr>
				</tbody>
			</table>

			<h3><?php esc_html_e( 'Recent activity', 'otwono-ai-connector' ); ?></h3>
			<p class="description">
				<?php esc_html_e(
					'Tokens, passwords and codes are removed before an entry is written, so this log never contains one.',
					'otwono-ai-connector'
				); ?>
			</p>
			<table class="widefat striped">
				<thead>
					<tr>
						<th scope="col"><?php esc_html_e( 'When', 'otwono-ai-connector' ); ?></th>
						<th scope="col"><?php esc_html_e( 'What', 'otwono-ai-connector' ); ?></th>
						<th scope="col"><?php esc_html_e( 'Outcome', 'otwono-ai-connector' ); ?></th>
					</tr>
				</thead>
				<tbody>
					<?php foreach ( array_reverse( array_slice( Logger::entries(), -20 ) ) as $entry ) : ?>
						<tr>
							<td><?php echo esc_html( (string) ( $entry['at'] ?? '' ) ); ?></td>
							<td><code><?php echo esc_html( (string) ( $entry['action'] ?? '' ) ); ?></code></td>
							<td><?php echo esc_html( (string) ( $entry['outcome'] ?? '' ) ); ?></td>
						</tr>
					<?php endforeach; ?>
				</tbody>
			</table>

			<hr>

			<h2><?php esc_html_e( 'Adding OTWONO to a page', 'otwono-ai-connector' ); ?></h2>
			<p><?php esc_html_e( 'Use a block from the editor, or one of these shortcodes:', 'otwono-ai-connector' ); ?></p>
			<ul>
				<li><code>[otwono_login]</code> — <?php esc_html_e( 'connect an OTWONO account', 'otwono-ai-connector' ); ?></li>
				<li><code>[otwono_profile]</code> — <?php esc_html_e( 'edit the member profile', 'otwono-ai-connector' ); ?></li>
				<li><code>[otwono_dashboard]</code> — <?php esc_html_e( 'synchronised projects', 'otwono-ai-connector' ); ?></li>
				<li><code>[otwono_marketplace]</code> — <?php esc_html_e( 'the human task marketplace', 'otwono-ai-connector' ); ?></li>
				<li><code>[otwono_status]</code> — <?php esc_html_e( 'connection status', 'otwono-ai-connector' ); ?></li>
			</ul>
		</div>
		<?php
	}
}
