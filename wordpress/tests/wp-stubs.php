<?php
/**
 * A minimal WordPress stand-in.
 *
 * The plugin's logic — sanitisation, redaction, capability checks, address
 * validation, uninstall behaviour — can be tested without a WordPress
 * installation. These stubs implement just enough of WordPress for that, and
 * record what the plugin asked them to do so the tests can assert on it.
 *
 * @package OTWONO\Connector\Tests
 */

declare( strict_types = 1 );

define( 'ABSPATH', __DIR__ . '/' );
define( 'HOUR_IN_SECONDS', 3600 );
define( 'MINUTE_IN_SECONDS', 60 );

// ---------------------------------------------------------------- state

final class WP_Test_State {
	public static array $options    = array();
	public static array $user_meta  = array();
	public static array $transients = array();
	public static array $roles      = array();
	public static array $requests   = array();
	public static array $responses  = array();
	public static int $current_user = 1;
	public static array $capabilities = array( 'otwono_use_connector' => true, 'manage_options' => true );

	public static function reset(): void {
		self::$options      = array();
		self::$user_meta    = array();
		self::$transients   = array();
		self::$roles        = array();
		self::$requests     = array();
		self::$responses    = array();
		self::$current_user = 1;
		self::$capabilities = array( 'otwono_use_connector' => true, 'manage_options' => true );
	}
}

final class WP_Error {
	private array $errors = array();
	private array $data   = array();

	public function __construct( string $code = '', string $message = '', mixed $data = null ) {
		if ( '' !== $code ) {
			$this->errors[ $code ] = array( $message );
			$this->data[ $code ]   = $data;
		}
	}

	public function get_error_code(): string {
		return (string) array_key_first( $this->errors );
	}

	public function get_error_message(): string {
		$code = $this->get_error_code();
		return $this->errors[ $code ][0] ?? '';
	}

	public function get_error_data(): mixed {
		return $this->data[ $this->get_error_code() ] ?? null;
	}
}

final class WP_Role {
	public array $capabilities = array();

	public function __construct( public string $name ) {}

	public function add_cap( string $cap ): void {
		$this->capabilities[ $cap ] = true;
	}

	public function remove_cap( string $cap ): void {
		unset( $this->capabilities[ $cap ] );
	}
}

final class WP_REST_Server {
	public const READABLE = 'GET';
	public const CREATABLE = 'POST';
	public const EDITABLE = 'POST, PUT, PATCH';
	public const DELETABLE = 'DELETE';
}

final class WP_REST_Response {
	public function __construct( public mixed $data = null, public int $status = 200 ) {}
	public function get_data(): mixed {
		return $this->data;
	}
}

final class WP_REST_Request {
	public function __construct( private array $params = array(), private array $json = array() ) {}
	public function get_param( string $key ): mixed {
		return $this->params[ $key ] ?? null;
	}
	public function get_json_params(): array {
		return $this->json;
	}
}

final class WP_Roles {
	public function get_names(): array {
		return array_combine(
			array_keys( WP_Test_State::$roles ),
			array_keys( WP_Test_State::$roles )
		) ?: array();
	}
}

// ------------------------------------------------------------- functions

function is_wp_error( mixed $thing ): bool {
	return $thing instanceof WP_Error;
}

function get_option( string $key, mixed $fallback = false ): mixed {
	return WP_Test_State::$options[ $key ] ?? $fallback;
}

function update_option( string $key, mixed $value, mixed $autoload = null ): bool {
	WP_Test_State::$options[ $key ] = $value;
	return true;
}

function add_option( string $key, mixed $value, string $deprecated = '', mixed $autoload = null ): bool {
	if ( array_key_exists( $key, WP_Test_State::$options ) ) {
		return false;
	}
	WP_Test_State::$options[ $key ] = $value;
	return true;
}

function delete_option( string $key ): bool {
	unset( WP_Test_State::$options[ $key ] );
	return true;
}

function get_transient( string $key ): mixed {
	return WP_Test_State::$transients[ $key ] ?? false;
}

function set_transient( string $key, mixed $value, int $ttl = 0 ): bool {
	WP_Test_State::$transients[ $key ] = $value;
	return true;
}

function get_user_meta( int $user_id, string $key, bool $single = false ): mixed {
	return WP_Test_State::$user_meta[ $user_id ][ $key ] ?? ( $single ? '' : array() );
}

function update_user_meta( int $user_id, string $key, mixed $value ): bool {
	WP_Test_State::$user_meta[ $user_id ][ $key ] = $value;
	return true;
}

function delete_user_meta( int $user_id, string $key ): bool {
	unset( WP_Test_State::$user_meta[ $user_id ][ $key ] );
	return true;
}

function get_current_user_id(): int {
	return WP_Test_State::$current_user;
}

function is_user_logged_in(): bool {
	return WP_Test_State::$current_user > 0;
}

function current_user_can( string $capability ): bool {
	return ! empty( WP_Test_State::$capabilities[ $capability ] );
}

function get_userdata( int $user_id ): object {
	return (object) array( 'display_name' => 'Test Person', 'ID' => $user_id );
}

function get_users( array $args = array() ): array {
	return array_keys( WP_Test_State::$user_meta );
}

function get_role( string $name ): ?WP_Role {
	return WP_Test_State::$roles[ $name ] ?? null;
}

function wp_roles(): WP_Roles {
	return new WP_Roles();
}

function home_url( string $path = '' ): string {
	return 'https://example-site.test' . $path;
}

function sanitize_text_field( string $value ): string {
	$value = strip_tags( $value );
	$value = preg_replace( '/[\r\n\t\0\x0B]/', '', $value ) ?? '';
	return trim( $value );
}

function sanitize_key( string $value ): string {
	return preg_replace( '/[^a-z0-9_\-]/', '', strtolower( $value ) ) ?? '';
}

function sanitize_email( string $value ): string {
	return (string) filter_var( trim( $value ), FILTER_SANITIZE_EMAIL );
}

function is_email( string $value ): string|false {
	return filter_var( $value, FILTER_VALIDATE_EMAIL ) ? $value : false;
}

function esc_url_raw( string $url, array $protocols = array( 'http', 'https' ) ): string {
	$url = trim( $url );
	if ( '' === $url ) {
		return '';
	}
	$scheme = strtolower( (string) parse_url( $url, PHP_URL_SCHEME ) );
	if ( ! in_array( $scheme, $protocols, true ) ) {
		return '';
	}
	return filter_var( $url, FILTER_VALIDATE_URL ) ? $url : '';
}

function esc_html( string $value ): string {
	return htmlspecialchars( $value, ENT_QUOTES, 'UTF-8' );
}

function esc_attr( string $value ): string {
	return htmlspecialchars( $value, ENT_QUOTES, 'UTF-8' );
}

function esc_textarea( string $value ): string {
	return htmlspecialchars( $value, ENT_QUOTES, 'UTF-8' );
}

function esc_url( string $value ): string {
	return esc_url_raw( $value );
}

function wp_kses_post( string $value ): string {
	return strip_tags( $value, '<p><br><strong><em><a><ul><ol><li>' );
}

function untrailingslashit( string $value ): string {
	return rtrim( $value, '/\\' );
}

function wp_parse_url( string $url, int $component = -1 ): mixed {
	return parse_url( $url, $component );
}

function wp_json_encode( mixed $value ): string|false {
	return json_encode( $value );
}

function wp_unslash( mixed $value ): mixed {
	return $value;
}

function wp_nonce_field( string $action, string $name = '_wpnonce' ): void {
	echo '<input type="hidden" name="' . esc_attr( $name ) . '" value="test-nonce">';
}

function wp_create_nonce( string $action ): string {
	return 'test-nonce-' . $action;
}

function checked( mixed $checked, mixed $current = true, bool $echo = true ): string {
	$result = (string) $checked === (string) $current || ( $checked && $current ) ? ' checked' : '';
	if ( $echo ) {
		echo $result;
	}
	return $result;
}

function selected( mixed $selected, mixed $current = true, bool $echo = true ): string {
	$result = (string) $selected === (string) $current ? ' selected' : '';
	if ( $echo ) {
		echo $result;
	}
	return $result;
}

function __( string $text, string $domain = '' ): string {
	return $text;
}

function esc_html__( string $text, string $domain = '' ): string {
	return esc_html( $text );
}

function esc_html_e( string $text, string $domain = '' ): void {
	echo esc_html( $text );
}

function _e( string $text, string $domain = '' ): void {
	echo $text;
}

function esc_attr__( string $text, string $domain = '' ): string {
	return esc_attr( $text );
}

function add_action( string $hook, mixed $callback, int $priority = 10, int $args = 1 ): bool {
	return true;
}

function add_filter( string $hook, mixed $callback, int $priority = 10, int $args = 1 ): bool {
	return true;
}

function add_shortcode( string $tag, mixed $callback ): void {}

function register_activation_hook( string $file, mixed $callback ): void {}

function register_deactivation_hook( string $file, mixed $callback ): void {}

function register_block_type( string $name, array $args = array() ): object {
	return (object) array( 'name' => $name, 'args' => $args );
}

function register_rest_route( string $namespace, string $route, array $args ): bool {
	WP_Test_State::$requests[] = array( 'namespace' => $namespace, 'route' => $route, 'args' => $args );
	return true;
}

function plugin_dir_path( string $file ): string {
	return dirname( $file ) . '/';
}

function plugin_dir_url( string $file ): string {
	return 'https://example-site.test/wp-content/plugins/otwono-ai-connector/';
}

function plugin_basename( string $file ): string {
	return 'otwono-ai-connector/otwono-ai-connector.php';
}

function wp_login_url( string $redirect = '' ): string {
	return 'https://example-site.test/wp-login.php';
}

function get_permalink(): string {
	return 'https://example-site.test/otwono/';
}

function wp_register_style( ...$args ): bool {
	return true;
}

function wp_register_script( ...$args ): bool {
	return true;
}

function wp_enqueue_style( string $handle ): void {}

function wp_enqueue_script( string $handle ): void {}

function wp_localize_script( string $handle, string $object, array $data ): bool {
	return true;
}

function load_plugin_textdomain( ...$args ): bool {
	return true;
}

function get_bloginfo( string $show = '' ): string {
	return '6.7';
}

function admin_url( string $path = '' ): string {
	return 'https://example-site.test/wp-admin/' . $path;
}

function add_query_arg( array $args, string $url ): string {
	return $url . '?' . http_build_query( $args );
}

/** Outbound HTTP is stubbed: tests queue responses and assert on requests. */
function wp_remote_request( string $url, array $args = array() ): array|WP_Error {
	WP_Test_State::$requests[] = array( 'url' => $url, 'args' => $args );
	$next = array_shift( WP_Test_State::$responses );
	if ( null === $next ) {
		return array( 'response' => array( 'code' => 200 ), 'body' => '{}' );
	}
	if ( $next instanceof WP_Error ) {
		return $next;
	}
	return $next;
}

function wp_remote_retrieve_response_code( array|WP_Error $response ): int {
	return is_array( $response ) ? (int) ( $response['response']['code'] ?? 0 ) : 0;
}

function wp_remote_retrieve_body( array|WP_Error $response ): string {
	return is_array( $response ) ? (string) ( $response['body'] ?? '' ) : '';
}
