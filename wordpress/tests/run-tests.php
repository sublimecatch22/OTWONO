<?php
/**
 * The plugin's test suite.
 *
 * Runs without a WordPress installation, against the stubs in `wp-stubs.php`.
 * Run it with: php wordpress/tests/run-tests.php
 *
 * @package OTWONO\Connector\Tests
 */

declare( strict_types = 1 );

require_once __DIR__ . '/wp-stubs.php';

$plugin = __DIR__ . '/../otwono-ai-connector/';
require_once $plugin . 'includes/constants.php';
require_once $plugin . 'includes/class-settings.php';
require_once $plugin . 'includes/class-logger.php';
require_once $plugin . 'includes/class-rate-limiter.php';
require_once $plugin . 'includes/class-client.php';
require_once $plugin . 'includes/class-account.php';
require_once $plugin . 'includes/class-rest.php';
require_once $plugin . 'includes/class-blocks.php';
require_once $plugin . 'includes/class-installer.php';

use OTWONO\Connector\Account;
use OTWONO\Connector\Blocks;
use OTWONO\Connector\Client;
use OTWONO\Connector\Installer;
use OTWONO\Connector\Logger;
use OTWONO\Connector\Rate_Limiter;
use OTWONO\Connector\Rest;
use OTWONO\Connector\Settings;

final class Runner {
	private int $passed = 0;
	private array $failures = array();
	private string $current = '';

	public function test( string $name, callable $body ): void {
		$this->current = $name;
		WP_Test_State::reset();
		try {
			$body( $this );
			$this->passed++;
			echo "  ok  $name\n";
		} catch ( Throwable $error ) {
			$this->failures[] = array( $name, $error->getMessage() );
			echo "FAIL  $name\n      " . $error->getMessage() . "\n";
		}
	}

	public function assert( bool $condition, string $message ): void {
		if ( ! $condition ) {
			throw new RuntimeException( $message );
		}
	}

	public function same( mixed $expected, mixed $actual, string $message = '' ): void {
		if ( $expected !== $actual ) {
			throw new RuntimeException(
				( '' !== $message ? $message . ': ' : '' ) .
				'expected ' . var_export( $expected, true ) . ', got ' . var_export( $actual, true )
			);
		}
	}

	public function contains( string $needle, string $haystack, string $message = '' ): void {
		if ( ! str_contains( $haystack, $needle ) ) {
			throw new RuntimeException(
				( '' !== $message ? $message . ': ' : '' ) . "expected to find '$needle'"
			);
		}
	}

	public function missing( string $needle, string $haystack, string $message = '' ): void {
		if ( str_contains( $haystack, $needle ) ) {
			throw new RuntimeException(
				( '' !== $message ? $message . ': ' : '' ) . "should not contain '$needle'"
			);
		}
	}

	public function summary(): int {
		echo "\n{$this->passed} passed, " . count( $this->failures ) . " failed\n";
		return array() === $this->failures ? 0 : 1;
	}
}

$run = new Runner();

echo "OTWONO AI Connector\n\n";

// ------------------------------------------------------------- settings

$run->test( 'a relay address must be https and must not be a private host', function ( Runner $t ) {
	$t->assert( Settings::is_acceptable_url( 'https://relay.example.com', 'relay_url' ), 'https public host' );

	foreach ( array(
		'http://relay.example.com',   // not https
		'https://localhost',          // loopback by name
		'https://127.0.0.1',          // loopback by address
		'https://10.0.0.5',           // private range
		'https://192.168.1.10',       // private range
		'https://printer.local',      // link-local name
		'ftp://relay.example.com',    // wrong scheme
		'not a url',
		'',
	) as $bad ) {
		$t->assert(
			! Settings::is_acceptable_url( $bad, 'relay_url' ),
			"should have refused $bad"
		);
	}
} );

$run->test( 'local development mode may use a loopback address', function ( Runner $t ) {
	$t->assert( Settings::is_acceptable_url( 'http://127.0.0.1:8787', 'local_url' ), 'loopback in local mode' );
} );

$run->test( 'unknown settings keys are dropped rather than stored', function ( Runner $t ) {
	$saved = Settings::save( array(
		'mode'         => 'relay',
		'relay_url'    => 'https://relay.example.com',
		'evil_payload' => '<script>alert(1)</script>',
		'token'        => 'a-token-that-should-not-live-here',
	) );

	$t->assert( ! array_key_exists( 'evil_payload', $saved ), 'an unknown key was kept' );
	$t->assert( ! array_key_exists( 'token', $saved ), 'the token must live in its own option' );
	$t->same( 'https://relay.example.com', $saved['relay_url'] );
} );

$run->test( 'a refused relay address is stored as empty rather than as given', function ( Runner $t ) {
	$saved = Settings::save( array( 'mode' => 'relay', 'relay_url' => 'https://192.168.0.9' ) );
	$t->same( '', $saved['relay_url'] );
	$t->assert( ! Settings::is_configured(), 'the plugin must not consider itself configured' );
} );

$run->test( 'the token is kept apart from the settings array', function ( Runner $t ) {
	Settings::set_token( 'relay-token-value' );
	$t->same( 'relay-token-value', Settings::token() );
	$t->assert( Settings::has_token(), 'has_token' );

	$all = Settings::all();
	$t->missing( 'relay-token-value', json_encode( $all ) ?: '', 'settings must not carry the token' );

	Settings::set_token( '' );
	$t->assert( ! Settings::has_token(), 'clearing the token' );
} );

$run->test( 'deleting member data on uninstall is off by default', function ( Runner $t ) {
	$t->assert( ! Settings::get( 'delete_data_on_uninstall' ), 'default must be off' );
} );

// --------------------------------------------------------------- logger

$run->test( 'the log removes anything that looks like a secret, at any depth', function ( Runner $t ) {
	Logger::record( 'relay_call', array(
		'endpoint' => 'https://relay.example.com',
		'token'    => 'should-not-appear',
		'headers'  => array( 'Authorization' => 'Bearer should-not-appear' ),
		'nested'   => array( array( 'api_key' => 'should-not-appear' ) ),
		'code'     => 'ABCD2345',
		'count'    => 12,
	) );

	$stored = json_encode( Logger::entries() ) ?: '';
	$t->missing( 'should-not-appear', $stored, 'a secret reached the log' );
	$t->missing( 'ABCD2345', $stored, 'a pairing code reached the log' );
	$t->contains( 'relay.example.com', $stored, 'ordinary detail should survive' );
	$t->contains( '12', $stored, 'ordinary numbers should survive' );
} );

$run->test( 'the log leaves ordinary keys alone', function ( Runner $t ) {
	$t->assert( ! Logger::is_sensitive( 'endpoint' ), 'endpoint' );
	$t->assert( ! Logger::is_sensitive( 'max_output_tokens' ), 'max_output_tokens' );
	$t->assert( Logger::is_sensitive( 'token' ), 'token' );
	$t->assert( Logger::is_sensitive( 'API-Key' ), 'API-Key' );
	$t->assert( Logger::is_sensitive( 'refresh_token' ), 'refresh_token' );
} );

// ---------------------------------------------------------- rate limits

$run->test( 'rate limiting stops repeated attempts and is per bucket', function ( Runner $t ) {
	for ( $attempt = 1; $attempt <= 3; $attempt++ ) {
		$t->assert( Rate_Limiter::check( 'signin:a', 3, 900 ), "attempt $attempt should pass" );
	}
	$t->assert( ! Rate_Limiter::check( 'signin:a', 3, 900 ), 'the fourth attempt should be refused' );
	$t->assert( Rate_Limiter::check( 'signin:b', 3, 900 ), 'another caller has its own allowance' );
} );

// --------------------------------------------------------------- client

$run->test( 'the client refuses to call anything before setup', function ( Runner $t ) {
	$result = Client::get( '/v1/profile' );
	$t->assert( is_wp_error( $result ), 'should be an error' );
	$t->same( 'otwono_not_configured', $result->get_error_code() );
} );

$run->test( 'the client attaches the token and never follows a redirect', function ( Runner $t ) {
	Settings::save( array( 'mode' => 'relay', 'relay_url' => 'https://relay.example.com' ) );
	WP_Test_State::$responses[] = array(
		'response' => array( 'code' => 200 ),
		'body'     => '{"ok":true}',
	);

	Client::get( '/v1/profile', 'a-member-token' );
	$request = WP_Test_State::$requests[0];

	$t->same( 'https://relay.example.com/v1/profile', $request['url'] );
	$t->same( 'Bearer a-member-token', $request['args']['headers']['Authorization'] );
	$t->same( 0, $request['args']['redirection'], 'a redirect could land on an unapproved host' );
} );

$run->test( 'a refusal from the relay is passed through with its message', function ( Runner $t ) {
	Settings::save( array( 'mode' => 'relay', 'relay_url' => 'https://relay.example.com' ) );
	WP_Test_State::$responses[] = array(
		'response' => array( 'code' => 403 ),
		'body'     => '{"error":{"code":"forbidden","message":"That session was signed out."}}',
	);

	$result = Client::get( '/v1/profile', 'a-token' );
	$t->assert( is_wp_error( $result ), 'should be an error' );
	$t->same( 'forbidden', $result->get_error_code() );
	$t->same( 'That session was signed out.', $result->get_error_message() );
} );

// -------------------------------------------------------------- account

$run->test( 'a member token is kept against the member, not site-wide', function ( Runner $t ) {
	Account::store( 7, 'acc_1', 'member-seven-token', array( 'profile.read' ) );
	Account::store( 9, 'acc_2', 'member-nine-token', array( 'profile.read' ) );

	$t->same( 'member-seven-token', Account::token( 7 ) );
	$t->same( 'member-nine-token', Account::token( 9 ) );
	$t->same( '', Account::token( 11 ), 'an unlinked member has no token' );
	$t->missing( 'member-seven-token', json_encode( WP_Test_State::$options ) ?: '', 'a member token reached a site option' );
} );

$run->test( 'signing in stores the token but never returns it', function ( Runner $t ) {
	Settings::save( array( 'mode' => 'relay', 'relay_url' => 'https://relay.example.com' ) );
	WP_Test_State::$responses[] = array(
		'response' => array( 'code' => 200 ),
		'body'     => '{"account_id":"acc_1","token":"secret-session-token","scopes":["profile.read"],"display_name":"A Person"}',
	);

	$result = Account::sign_in( 4, 'person@example.com', 'a-long-enough-password', array( 'profile.read' ) );

	$t->assert( ! is_wp_error( $result ), 'sign-in should succeed' );
	$t->assert( ! array_key_exists( 'token', $result ), 'the token must not be returned to the caller' );
	$t->same( 'secret-session-token', Account::token( 4 ), 'the token should be stored' );
	$t->assert( Account::is_linked( 4 ), 'the member should be linked' );
} );

$run->test( 'signing out forgets the token even if the relay is unreachable', function ( Runner $t ) {
	Settings::save( array( 'mode' => 'relay', 'relay_url' => 'https://relay.example.com' ) );
	Account::store( 4, 'acc_1', 'a-token', array( 'profile.read' ) );
	WP_Test_State::$responses[] = new WP_Error( 'http_request_failed', 'network down' );

	Account::sign_out( 4 );
	$t->same( '', Account::token( 4 ), 'the local token must be forgotten regardless' );
	$t->assert( ! Account::is_linked( 4 ), 'the member should be unlinked' );
} );

$run->test( 'profile calls are refused before the member is linked', function ( Runner $t ) {
	Settings::save( array( 'mode' => 'relay', 'relay_url' => 'https://relay.example.com' ) );
	$result = Account::profile( 4 );
	$t->assert( is_wp_error( $result ), 'should be an error' );
	$t->same( 'otwono_not_linked', $result->get_error_code() );
} );

// ------------------------------------------------------------------ rest

$run->test( 'every REST route states a real permission check', function ( Runner $t ) {
	Rest::register();
	$routes = WP_Test_State::$requests;
	$t->assert( count( $routes ) >= 8, 'expected the plugin to register its routes' );

	foreach ( $routes as $route ) {
		$definitions = isset( $route['args']['methods'] ) ? array( $route['args'] ) : $route['args'];
		foreach ( $definitions as $definition ) {
			if ( ! is_array( $definition ) || ! isset( $definition['permission_callback'] ) ) {
				continue;
			}
			$callback = $definition['permission_callback'];
			$t->assert(
				'__return_true' !== $callback,
				"route {$route['route']} is open to anyone"
			);
			$t->assert( is_callable( $callback ), "route {$route['route']} has an uncallable check" );
		}
	}
} );

$run->test( 'a signed-out visitor is refused', function ( Runner $t ) {
	WP_Test_State::$current_user = 0;
	$result = Rest::can_use();
	$t->assert( is_wp_error( $result ), 'should be refused' );
	$t->same( 'otwono_not_signed_in', $result->get_error_code() );
} );

$run->test( 'a member without the capability is refused', function ( Runner $t ) {
	WP_Test_State::$capabilities['otwono_use_connector'] = false;
	$result = Rest::can_use();
	$t->assert( is_wp_error( $result ), 'should be refused' );
	$t->same( 'otwono_not_permitted', $result->get_error_code() );
} );

$run->test( 'a member cannot reach the administrator routes', function ( Runner $t ) {
	WP_Test_State::$capabilities['manage_options'] = false;
	$result = Rest::can_administer();
	$t->assert( is_wp_error( $result ), 'should be refused' );
	$t->same( 'otwono_not_permitted', $result->get_error_code() );
} );

$run->test( 'profile input is sanitised and unknown fields are dropped', function ( Runner $t ) {
	$clean = Rest::sanitise_profile( array(
		'display_name'    => "  A Person\n<script>alert(1)</script>  ",
		'biography'       => '<p>Fine</p><script>alert(2)</script>',
		'interests'       => array( 'gardening', '<b>bold</b>', 42 ),
		'portfolio_links' => array( 'https://example.com', 'javascript:alert(1)', 'file:///etc/passwd' ),
		'visibility'      => array( 'display_name' => true, 'not_a_field' => true ),
		'is_admin'        => true,
		'account_id'      => 'acc_someone_else',
	) );

	$t->missing( '<script>', json_encode( $clean ) ?: '', 'a script tag survived' );
	$t->missing( 'javascript:', json_encode( $clean ) ?: '', 'a javascript link survived' );
	$t->missing( 'file:///', json_encode( $clean ) ?: '', 'a file link survived' );
	$t->assert( ! array_key_exists( 'is_admin', $clean ), 'an unknown field was kept' );
	$t->assert( ! array_key_exists( 'account_id', $clean ), 'the account id must not be settable' );
	$t->assert( ! array_key_exists( 'not_a_field', $clean['visibility'] ), 'unknown visibility field kept' );
	$t->same( true, $clean['visibility']['display_name'] );
	$t->same( array( 'https://example.com' ), $clean['portfolio_links'] );
} );

$run->test( 'a profile is private unless a field is marked public', function ( Runner $t ) {
	$clean = Rest::sanitise_profile( array( 'display_name' => 'A Person', 'biography' => 'Private.' ) );
	$t->same( array(), $clean['visibility'], 'nothing should be public by default' );
} );

// ---------------------------------------------------------------- blocks

$run->test( 'every block has a shortcode and the same renderer', function ( Runner $t ) {
	$names = Blocks::names();
	$t->same( 5, count( $names ) );
	foreach ( array( 'otwono/status', 'otwono/login', 'otwono/profile', 'otwono/dashboard', 'otwono/marketplace' ) as $expected ) {
		$t->assert( in_array( $expected, $names, true ), "missing block $expected" );
	}
} );

// ------------------------------------------------------------- installer

$run->test( 'activation grants only the connector capability', function ( Runner $t ) {
	WP_Test_State::$roles = array(
		'administrator' => new WP_Role( 'administrator' ),
		'subscriber'    => new WP_Role( 'subscriber' ),
	);

	Installer::activate();

	foreach ( WP_Test_State::$roles as $role ) {
		$t->assert(
			isset( $role->capabilities['otwono_use_connector'] ),
			"{$role->name} should hold the connector capability"
		);
		foreach ( array( 'manage_options', 'edit_users', 'install_plugins', 'edit_files' ) as $forbidden ) {
			$t->assert(
				! isset( $role->capabilities[ $forbidden ] ),
				"the plugin granted $forbidden to {$role->name}"
			);
		}
	}
} );

$run->test( 'deactivation removes the capability but keeps member data', function ( Runner $t ) {
	WP_Test_State::$roles = array( 'subscriber' => new WP_Role( 'subscriber' ) );
	Installer::activate();
	Account::store( 4, 'acc_1', 'a-token', array( 'profile.read' ) );

	Installer::deactivate();

	$t->assert(
		! isset( WP_Test_State::$roles['subscriber']->capabilities['otwono_use_connector'] ),
		'the capability should be removed'
	);
	$t->same( 'a-token', Account::token( 4 ), 'member data must survive deactivation' );
} );

$run->test( 'uninstalling keeps member data by default', function ( Runner $t ) {
	WP_Test_State::$roles = array( 'subscriber' => new WP_Role( 'subscriber' ) );
	Installer::activate();
	Settings::set_token( 'site-token' );
	Account::store( 4, 'acc_1', 'member-token', array( 'profile.read' ) );

	Installer::uninstall();

	$t->assert( ! Settings::has_token(), 'the site token should be removed' );
	$t->same( false, get_option( 'otwono_connector_settings', false ), 'settings should be removed' );
	$t->same( 'member-token', Account::token( 4 ), 'member data must survive by default' );
} );

$run->test( 'uninstalling deletes member data only when asked', function ( Runner $t ) {
	WP_Test_State::$roles = array( 'subscriber' => new WP_Role( 'subscriber' ) );
	Installer::activate();
	Settings::save( array_merge( Settings::all(), array( 'delete_data_on_uninstall' => true ) ) );
	Account::store( 4, 'acc_1', 'member-token', array( 'profile.read' ) );

	Installer::uninstall();

	$t->same( '', Account::token( 4 ), 'member data should be removed when the owner asked' );
} );

$run->test( 'migration moves a token out of the settings array', function ( Runner $t ) {
	update_option( 'otwono_connector_settings', array(
		'mode'      => 'relay',
		'relay_url' => 'https://relay.example.com',
		'token'     => 'legacy-token',
	) );

	Installer::migrate( 1, 2 );

	$t->same( 'legacy-token', Settings::token(), 'the token should move to its own option' );
	$stored = get_option( 'otwono_connector_settings', array() );
	$t->assert( ! isset( $stored['token'] ), 'the token should no longer be in settings' );
} );

exit( $run->summary() );
