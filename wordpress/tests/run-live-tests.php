<?php
/**
 * The plugin against a relay that is really running.
 *
 * The ordinary suite stubs outbound HTTP. This one does not: the same plugin
 * code talks to the relay binary over the network, so the pairing, sign-in,
 * profile and synchronised-metadata path is exercised for real, including the
 * relay's own scope checks and privacy rules.
 *
 * Start it with `scripts/run-wordpress-live-tests.sh`, which starts a relay
 * against a throwaway database and sets OTWONO_RELAY_URL.
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
require_once $plugin . 'includes/class-shortcodes.php';

use OTWONO\Connector\Account;
use OTWONO\Connector\Rest;
use OTWONO\Connector\Settings;
use OTWONO\Connector\Shortcodes;

$relay = getenv( 'OTWONO_RELAY_URL' );
if ( ! is_string( $relay ) || '' === $relay ) {
	fwrite( STDERR, "OTWONO_RELAY_URL is not set. Use scripts/run-wordpress-live-tests.sh\n" );
	exit( 2 );
}

/** Readable text for an assertion message, WP_Error included. */
function describe( mixed $value ): string {
	if ( $value instanceof WP_Error ) {
		return 'WP_Error(' . $value->get_error_code() . '): ' . $value->get_error_message();
	}
	return (string) wp_json_encode( $value );
}

/** A request made as the desktop application would make it. */
function desktop( string $method, string $path, array $body = array(), string $token = '' ): array {
	global $relay;
	$handle  = curl_init( $relay . $path );
	$headers = array( 'Content-Type: application/json' );
	if ( '' !== $token ) {
		$headers[] = 'Authorization: Bearer ' . $token;
	}
	curl_setopt_array(
		$handle,
		array(
			CURLOPT_CUSTOMREQUEST  => $method,
			CURLOPT_RETURNTRANSFER => true,
			CURLOPT_HTTPHEADER     => $headers,
			CURLOPT_POSTFIELDS     => wp_json_encode( $body ),
			CURLOPT_TIMEOUT        => 15,
		)
	);
	$raw  = curl_exec( $handle );
	$code = (int) curl_getinfo( $handle, CURLINFO_RESPONSE_CODE );
	curl_close( $handle );
	$decoded = json_decode( (string) $raw, true );
	return array( 'code' => $code, 'body' => is_array( $decoded ) ? $decoded : array() );
}

final class LiveRunner {
	private int $passed = 0;
	private array $failures = array();

	public function test( string $name, callable $body ): void {
		WP_Test_State::reset();
		WP_Test_State::$live_http = true;
		// The settings form refuses a private address; this writes the option
		// directly, because the relay under test is running on this machine.
		update_option(
			'otwono_connector_settings',
			array( 'mode' => 'relay', 'relay_url' => getenv( 'OTWONO_RELAY_URL' ) )
		);
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
		echo "\n" . $this->passed . ' passed, ' . count( $this->failures ) . " failed\n";
		return empty( $this->failures ) ? 0 : 1;
	}
}

/** Make an account on the relay the way the desktop application does. */
function make_account( string $suffix ): array {
	$email    = 'owner+' . $suffix . '@example.test';
	$password = 'a-long-enough-passphrase-' . $suffix;

	$created = desktop( 'POST', '/v1/accounts', array(
		'email'        => $email,
		'password'     => $password,
		'display_name' => 'Sam Owner',
	) );
	if ( 200 !== $created['code'] ) {
		throw new RuntimeException( 'the relay refused the registration: ' . json_encode( $created ) );
	}

	$signed = desktop( 'POST', '/v1/accounts/sign-in', array(
		'email'        => $email,
		'password'     => $password,
		'device_label' => 'Desktop',
		// The desktop application holds every scope; a paired site does not.
		'scopes'       => array(
			'profile.read',
			'profile.write',
			'projects.read',
			'projects.write',
			'marketplace.read',
		),
	) );
	if ( 200 !== $signed['code'] ) {
		throw new RuntimeException( 'the relay refused the sign-in: ' . json_encode( $signed ) );
	}

	return array(
		'email'      => $email,
		'password'   => $password,
		'account_id' => $signed['body']['account_id'],
		'token'      => $signed['body']['token'],
	);
}

$run = new LiveRunner();

$run->test( 'the relay is reachable and says what it is', function ( LiveRunner $t ) {
	$health = \OTWONO\Connector\Client::health();
	$t->assert( ! is_wp_error( $health ), 'the relay should answer its health check' );
} );

$run->test( 'a pairing code from the desktop application links the site', function ( LiveRunner $t ) {
	$account = make_account( 'pairing' );
	$pairing = desktop( 'POST', '/v1/pairings', array(), $account['token'] );
	$t->same( 200, $pairing['code'], 'the desktop application should get a pairing code' );

	$request = new WP_REST_Request( array( 'code' => $pairing['body']['code'] ) );
	$result  = Rest::pair( $request );

	$t->assert( ! is_wp_error( $result ), 'pairing should succeed: ' . json_encode( $result ) );
	$t->same( true, $result->get_data()['paired'] );
	$t->assert( Settings::has_token(), 'the site should now hold a token' );

	// The code is single use, so a stolen one is worth nothing.
	$again = Rest::pair( new WP_REST_Request( array( 'code' => $pairing['body']['code'] ) ) );
	$t->assert( is_wp_error( $again ), 'a pairing code must not work twice' );
} );

$run->test( 'a member signs in, edits their profile and reads it back', function ( LiveRunner $t ) {
	$account = make_account( 'profile' );

	$signed = Account::sign_in( 7, $account['email'], $account['password'], array( 'profile.read', 'profile.write' ) );
	$t->assert( ! is_wp_error( $signed ), 'sign-in should succeed: ' . describe( $signed ) );
	$t->assert( Account::is_linked( 7 ), 'the member should be linked' );
	$t->assert( ! isset( $signed['token'] ), 'the token must never be returned to the browser' );

	$saved = Account::save_profile( 7, array(
		'display_name'    => 'Sam Owner',
		'biography'       => 'I test things for a living.',
		'interests'       => array( 'testing', 'documentation' ),
		'capabilities'    => array( 'writing' ),
		'portfolio_links' => array( 'https://example.test/portfolio' ),
		'visibility'      => array( 'display_name' => true, 'biography' => true ),
		'is_ai_identity'  => false,
	) );
	$t->assert( ! is_wp_error( $saved ), 'saving the profile should succeed: ' . describe( $saved ) );

	$read = Account::profile( 7 );
	$t->assert( ! is_wp_error( $read ), 'reading the profile should succeed' );
	$t->same( 'I test things for a living.', $read['biography'] );
	$t->same( array( 'testing', 'documentation' ), $read['interests'] );

	// What the public sees is only what the member marked public.
	$public = desktop( 'GET', '/v1/profiles/' . $account['account_id'] );
	$t->same( 200, $public['code'] );
	$fields = $public['body']['fields'];
	$t->same( 'Sam Owner', $fields['display_name'] );
	$t->same( 'I test things for a living.', $fields['biography'] );
	$t->assert(
		! array_key_exists( 'capabilities', $fields ),
		'a field left private must not appear in the public profile'
	);
	$t->assert(
		! array_key_exists( 'portfolio_links', $fields ),
		'nothing is public unless the member said so'
	);
} );

$run->test( 'synchronised project metadata is visible, and content is not', function ( LiveRunner $t ) {
	$account = make_account( 'projects' );

	// The desktop application pushes metadata for a project the user chose.
	$pushed = desktop( 'POST', '/v1/projects', array(
		'projects' => array(
			array(
				'id'              => 'prj_live_1',
				'title'           => 'Quarterly report for Q3',
				'state'           => 'completed',
				'task_count'      => 2,
				'completed_tasks' => 2,
			),
		),
	), $account['token'] );
	$t->same( 200, $pushed['code'], 'the relay should accept project metadata' );

	Account::sign_in( 9, $account['email'], $account['password'], array( 'projects.read' ) );
	$projects = Account::projects( 9 );
	$t->assert( ! is_wp_error( $projects ), 'the member should see their projects' );
	$t->same( 1, count( $projects ) );
	$t->same( 'Quarterly report for Q3', $projects[0]['title'] );
	$t->same( 'completed', $projects[0]['state'] );

	// The relay has no field for content, so none can come back.
	$serialised = json_encode( $projects[0] );
	$t->missing( 'objective', $serialised, 'the relay must not carry project content' );
	$t->missing( 'output', $serialised );
} );

$run->test( 'a scope the member did not grant is refused by the relay', function ( LiveRunner $t ) {
	$account = make_account( 'scopes' );
	Account::sign_in( 11, $account['email'], $account['password'], array( 'profile.read' ) );

	$refused = Account::save_profile( 11, array( 'display_name' => 'Should not save' ) );
	$t->assert( is_wp_error( $refused ), 'writing without profile.write must be refused' );
} );

$run->test( 'the profile block renders what the member published', function ( LiveRunner $t ) {
	$account = make_account( 'block' );
	Account::sign_in( 13, $account['email'], $account['password'], array( 'profile.read', 'profile.write' ) );
	Account::save_profile( 13, array(
		'display_name' => 'Sam Owner',
		'biography'    => 'Renders in a block.',
		'visibility'   => array( 'display_name' => true, 'biography' => true ),
	) );

	WP_Test_State::$current_user = 13;
	$html = Shortcodes::profile( array() );
	$t->contains( 'Renders in a block.', $html );
	$t->missing( '<script', $html, 'rendered output must not carry script tags' );
} );

exit( $run->summary() );
