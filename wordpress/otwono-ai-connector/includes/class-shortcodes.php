<?php
/**
 * Shortcodes, and the renderers behind them.
 *
 * The blocks in `class-blocks.php` call the same renderers, so there is one
 * implementation per surface rather than two that can drift apart.
 *
 * Everything user-supplied is escaped at the point of output.
 *
 * @package OTWONO\Connector
 */

declare( strict_types = 1 );

namespace OTWONO\Connector;

defined( 'ABSPATH' ) || exit;

final class Shortcodes {

	public static function hooks(): void {
		add_shortcode( 'otwono_status', array( self::class, 'status' ) );
		add_shortcode( 'otwono_login', array( self::class, 'login' ) );
		add_shortcode( 'otwono_profile', array( self::class, 'profile' ) );
		add_shortcode( 'otwono_dashboard', array( self::class, 'dashboard' ) );
		add_shortcode( 'otwono_marketplace', array( self::class, 'marketplace' ) );
		add_action( 'wp_enqueue_scripts', array( self::class, 'enqueue' ) );
	}

	public static function enqueue(): void {
		wp_register_style(
			'otwono-connector',
			OTWONO_CONNECTOR_URL . 'public/connector.css',
			array(),
			VERSION
		);
		wp_register_script(
			'otwono-connector',
			OTWONO_CONNECTOR_URL . 'public/connector.js',
			array( 'wp-api-fetch' ),
			VERSION,
			true
		);
		wp_localize_script(
			'otwono-connector',
			'otwonoConnector',
			array(
				'namespace' => Rest::NAMESPACE,
				'nonce'     => wp_create_nonce( 'wp_rest' ),
				'strings'   => array(
					'signedOut' => __( 'You are signed out of OTWONO.', 'otwono-ai-connector' ),
					'failed'    => __( 'That did not work.', 'otwono-ai-connector' ),
					'saved'     => __( 'Saved.', 'otwono-ai-connector' ),
				),
			)
		);
	}

	private static function assets(): void {
		wp_enqueue_style( 'otwono-connector' );
		wp_enqueue_script( 'otwono-connector' );
	}

	/** A message inside the plugin's own wrapper, so styling is consistent. */
	private static function notice( string $message, string $tone = 'info' ): string {
		return sprintf(
			'<div class="otwono otwono--%1$s"><p>%2$s</p></div>',
			esc_attr( $tone ),
			esc_html( $message )
		);
	}

	private static function require_signed_in(): ?string {
		if ( ! is_user_logged_in() ) {
			return sprintf(
				'<div class="otwono otwono--info"><p>%1$s</p><p><a class="otwono__button" href="%2$s">%3$s</a></p></div>',
				esc_html__( 'Sign in to this site to use OTWONO.', 'otwono-ai-connector' ),
				esc_url( wp_login_url( get_permalink() ?: home_url() ) ),
				esc_html__( 'Sign in', 'otwono-ai-connector' )
			);
		}
		if ( ! current_user_can( CAPABILITY ) ) {
			return self::notice(
				__( 'Your account is not allowed to use OTWONO on this site.', 'otwono-ai-connector' ),
				'caution'
			);
		}
		if ( ! Settings::is_configured() ) {
			return self::notice(
				__( 'OTWONO is not connected on this site yet. An administrator needs to finish setup.', 'otwono-ai-connector' ),
				'caution'
			);
		}
		return null;
	}

	public static function status(): string {
		self::assets();

		$configured = Settings::is_configured();
		$paired     = Settings::has_token();
		$linked     = is_user_logged_in() && Account::is_linked( get_current_user_id() );

		$rows = array(
			array( __( 'Site connected to OTWONO', 'otwono-ai-connector' ), $configured ),
			array( __( 'Site paired with an OTWONO account', 'otwono-ai-connector' ), $paired ),
			array( __( 'Your account linked', 'otwono-ai-connector' ), $linked ),
		);

		$html = '<div class="otwono otwono--card"><h3>' .
			esc_html__( 'OTWONO connection', 'otwono-ai-connector' ) . '</h3><ul class="otwono__status">';

		foreach ( $rows as [$label, $state] ) {
			$html .= sprintf(
				'<li><span class="otwono__dot otwono__dot--%1$s" aria-hidden="true"></span>%2$s — <strong>%3$s</strong></li>',
				$state ? 'on' : 'off',
				esc_html( $label ),
				$state
					? esc_html__( 'yes', 'otwono-ai-connector' )
					: esc_html__( 'no', 'otwono-ai-connector' )
			);
		}

		$html .= '</ul><p class="otwono__muted">' . esc_html__(
			'This site sees only your profile and the projects you chose to synchronise. Your conversations, files and knowledge stay on your own machine.',
			'otwono-ai-connector'
		) . '</p></div>';

		return $html;
	}

	public static function login(): string {
		self::assets();
		$blocked = self::require_signed_in();
		if ( null !== $blocked ) {
			return $blocked;
		}

		$user_id = get_current_user_id();
		if ( Account::is_linked( $user_id ) ) {
			return sprintf(
				'<div class="otwono otwono--card"><h3>%1$s</h3><p>%2$s</p>
				 <button type="button" class="otwono__button" data-otwono-action="sign-out">%3$s</button></div>',
				esc_html__( 'OTWONO account', 'otwono-ai-connector' ),
				esc_html__( 'Your OTWONO account is connected to this site.', 'otwono-ai-connector' ),
				esc_html__( 'Disconnect', 'otwono-ai-connector' )
			);
		}

		$allow_registration = (bool) Settings::get( 'allow_registration', true );

		ob_start();
		?>
		<div class="otwono otwono--card">
			<h3><?php esc_html_e( 'Connect your OTWONO account', 'otwono-ai-connector' ); ?></h3>
			<form class="otwono__form" data-otwono-form="sign-in">
				<?php wp_nonce_field( 'otwono_sign_in', 'otwono_nonce' ); ?>
				<p class="otwono__field">
					<label for="otwono-email"><?php esc_html_e( 'Email address', 'otwono-ai-connector' ); ?></label>
					<input type="email" id="otwono-email" name="email" autocomplete="email" required>
				</p>
				<p class="otwono__field">
					<label for="otwono-password"><?php esc_html_e( 'Password', 'otwono-ai-connector' ); ?></label>
					<input type="password" id="otwono-password" name="password" autocomplete="current-password" required>
				</p>
				<p>
					<button type="submit" class="otwono__button otwono__button--primary">
						<?php esc_html_e( 'Connect', 'otwono-ai-connector' ); ?>
					</button>
					<?php if ( $allow_registration ) : ?>
						<button type="button" class="otwono__button" data-otwono-action="register">
							<?php esc_html_e( 'Create an account', 'otwono-ai-connector' ); ?>
						</button>
					<?php endif; ?>
				</p>
				<p class="otwono__message" role="status" aria-live="polite"></p>
			</form>
			<p class="otwono__muted">
				<?php esc_html_e(
					'Your password is sent to OTWONO to sign in and is never stored on this site.',
					'otwono-ai-connector'
				); ?>
			</p>
		</div>
		<?php
		return (string) ob_get_clean();
	}

	public static function profile(): string {
		self::assets();
		$blocked = self::require_signed_in();
		if ( null !== $blocked ) {
			return $blocked;
		}

		$user_id = get_current_user_id();
		if ( ! Account::is_linked( $user_id ) ) {
			return self::notice(
				__( 'Connect your OTWONO account to edit your profile.', 'otwono-ai-connector' ),
				'info'
			);
		}

		$profile = Account::profile( $user_id );
		if ( is_wp_error( $profile ) ) {
			return self::notice( $profile->get_error_message(), 'caution' );
		}

		$visibility = is_array( $profile['visibility'] ?? null ) ? $profile['visibility'] : array();

		ob_start();
		?>
		<div class="otwono otwono--card">
			<h3><?php esc_html_e( 'Your OTWONO profile', 'otwono-ai-connector' ); ?></h3>
			<form class="otwono__form" data-otwono-form="profile">
				<?php wp_nonce_field( 'otwono_profile', 'otwono_nonce' ); ?>
				<p class="otwono__field">
					<label for="otwono-display-name"><?php esc_html_e( 'Display name', 'otwono-ai-connector' ); ?></label>
					<input type="text" id="otwono-display-name" name="display_name"
						value="<?php echo esc_attr( (string) ( $profile['display_name'] ?? '' ) ); ?>">
					<label class="otwono__check">
						<input type="checkbox" name="visible_display_name"
							<?php checked( ! empty( $visibility['display_name'] ) ); ?>>
						<?php esc_html_e( 'Show this publicly', 'otwono-ai-connector' ); ?>
					</label>
				</p>
				<p class="otwono__field">
					<label for="otwono-biography"><?php esc_html_e( 'About you', 'otwono-ai-connector' ); ?></label>
					<textarea id="otwono-biography" name="biography" rows="5"><?php
						echo esc_textarea( (string) ( $profile['biography'] ?? '' ) );
					?></textarea>
					<label class="otwono__check">
						<input type="checkbox" name="visible_biography"
							<?php checked( ! empty( $visibility['biography'] ) ); ?>>
						<?php esc_html_e( 'Show this publicly', 'otwono-ai-connector' ); ?>
					</label>
				</p>
				<p class="otwono__field">
					<label for="otwono-interests"><?php esc_html_e( 'Interests', 'otwono-ai-connector' ); ?></label>
					<input type="text" id="otwono-interests" name="interests"
						value="<?php echo esc_attr( implode( ', ', (array) ( $profile['interests'] ?? array() ) ) ); ?>">
					<span class="otwono__muted"><?php esc_html_e( 'Separated by commas.', 'otwono-ai-connector' ); ?></span>
					<label class="otwono__check">
						<input type="checkbox" name="visible_interests"
							<?php checked( ! empty( $visibility['interests'] ) ); ?>>
						<?php esc_html_e( 'Show these publicly', 'otwono-ai-connector' ); ?>
					</label>
				</p>
				<p class="otwono__field">
					<label class="otwono__check">
						<input type="checkbox" name="is_ai_identity"
							<?php checked( ! empty( $profile['is_ai_identity'] ) ); ?>>
						<?php esc_html_e(
							'This profile is an AI identity, not a person',
							'otwono-ai-connector'
						); ?>
					</label>
					<span class="otwono__muted"><?php esc_html_e(
						'Anywhere this profile appears, it will say so. An AI is never presented as a person.',
						'otwono-ai-connector'
					); ?></span>
				</p>
				<p>
					<button type="submit" class="otwono__button otwono__button--primary">
						<?php esc_html_e( 'Save profile', 'otwono-ai-connector' ); ?>
					</button>
				</p>
				<p class="otwono__message" role="status" aria-live="polite"></p>
			</form>
			<p class="otwono__muted">
				<?php esc_html_e(
					'Everything is private unless you tick "show publicly".',
					'otwono-ai-connector'
				); ?>
			</p>
		</div>
		<?php
		return (string) ob_get_clean();
	}

	public static function dashboard(): string {
		self::assets();
		$blocked = self::require_signed_in();
		if ( null !== $blocked ) {
			return $blocked;
		}

		$user_id = get_current_user_id();
		if ( ! Account::is_linked( $user_id ) ) {
			return self::notice(
				__( 'Connect your OTWONO account to see your projects.', 'otwono-ai-connector' ),
				'info'
			);
		}

		$projects = Account::projects( $user_id );
		if ( is_wp_error( $projects ) ) {
			return self::notice( $projects->get_error_message(), 'caution' );
		}

		if ( array() === $projects ) {
			return self::notice(
				__( 'No projects are synchronised yet. In the OTWONO desktop application, switch on synchronisation for a project you want to see here.', 'otwono-ai-connector' ),
				'info'
			);
		}

		$html = '<div class="otwono otwono--card"><h3>' .
			esc_html__( 'Your projects', 'otwono-ai-connector' ) . '</h3><ul class="otwono__list">';

		foreach ( $projects as $project ) {
			if ( ! is_array( $project ) ) {
				continue;
			}
			$html .= sprintf(
				'<li><strong>%1$s</strong> <span class="otwono__badge">%2$s</span><br><span class="otwono__muted">%3$s</span></li>',
				esc_html( (string) ( $project['title'] ?? '' ) ),
				esc_html( str_replace( '_', ' ', (string) ( $project['state'] ?? '' ) ) ),
				esc_html(
					sprintf(
						/* translators: 1: completed tasks, 2: total tasks. */
						__( '%1$d of %2$d tasks done', 'otwono-ai-connector' ),
						(int) ( $project['completed_tasks'] ?? 0 ),
						(int) ( $project['task_count'] ?? 0 )
					)
				)
			);
		}

		$html .= '</ul><p class="otwono__muted">' . esc_html__(
			'Only titles and progress are synchronised. The work itself stays on your machine.',
			'otwono-ai-connector'
		) . '</p></div>';

		return $html;
	}

	public static function marketplace(): string {
		self::assets();
		$blocked = self::require_signed_in();
		if ( null !== $blocked ) {
			return $blocked;
		}

		$user_id = get_current_user_id();
		if ( ! Account::is_linked( $user_id ) ) {
			return self::notice(
				__( 'Connect your OTWONO account to see the marketplace.', 'otwono-ai-connector' ),
				'info'
			);
		}

		ob_start();
		?>
		<div class="otwono otwono--card">
			<h3><?php esc_html_e( 'Human task marketplace', 'otwono-ai-connector' ); ?></h3>
			<div class="otwono otwono--caution">
				<p><?php esc_html_e(
					'This marketplace is a development preview. Payments are simulated: no money moves and no worker is really paid.',
					'otwono-ai-connector'
				); ?></p>
			</div>
			<div data-otwono-region="listings" aria-live="polite">
				<p class="otwono__muted"><?php esc_html_e( 'Loading tasks…', 'otwono-ai-connector' ); ?></p>
			</div>
		</div>
		<?php
		return (string) ob_get_clean();
	}
}
