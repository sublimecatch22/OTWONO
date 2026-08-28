<?php
/**
 * The plugin's REST API.
 *
 * Every route states a capability check and validates its arguments. Nothing
 * here trusts the caller: `permission_callback` is never `__return_true` for a
 * route that reads or changes anything.
 *
 * @package OTWONO\Connector
 */

declare( strict_types = 1 );

namespace OTWONO\Connector;

use WP_Error;
use WP_REST_Request;
use WP_REST_Response;
use WP_REST_Server;

defined( 'ABSPATH' ) || exit;

final class Rest {

	public const NAMESPACE = 'otwono/v1';

	public static function hooks(): void {
		add_action( 'rest_api_init', array( self::class, 'register' ) );
	}

	/** A signed-in member with the connector capability. */
	public static function can_use(): bool|WP_Error {
		if ( ! is_user_logged_in() ) {
			return new WP_Error(
				'otwono_not_signed_in',
				__( 'Sign in to WordPress first.', 'otwono-ai-connector' ),
				array( 'status' => 401 )
			);
		}
		if ( ! current_user_can( CAPABILITY ) ) {
			return new WP_Error(
				'otwono_not_permitted',
				__( 'Your account is not allowed to use OTWONO on this site.', 'otwono-ai-connector' ),
				array( 'status' => 403 )
			);
		}
		return true;
	}

	/** An administrator, for settings and diagnostics. */
	public static function can_administer(): bool|WP_Error {
		if ( ! current_user_can( 'manage_options' ) ) {
			return new WP_Error(
				'otwono_not_permitted',
				__( 'Only an administrator can change these settings.', 'otwono-ai-connector' ),
				array( 'status' => 403 )
			);
		}
		return true;
	}

	public static function register(): void {
		register_rest_route(
			self::NAMESPACE,
			'/status',
			array(
				'methods'             => WP_REST_Server::READABLE,
				'callback'            => array( self::class, 'status' ),
				'permission_callback' => array( self::class, 'can_use' ),
			)
		);

		register_rest_route(
			self::NAMESPACE,
			'/account/register',
			array(
				'methods'             => WP_REST_Server::CREATABLE,
				'callback'            => array( self::class, 'register_account' ),
				'permission_callback' => array( self::class, 'can_use' ),
				'args'                => array(
					'email'    => array(
						'required'          => true,
						'type'              => 'string',
						'sanitize_callback' => 'sanitize_email',
						'validate_callback' => static fn( $value ) => is_email( (string) $value ) !== false,
					),
					'password' => array(
						'required' => true,
						'type'     => 'string',
					),
				),
			)
		);

		register_rest_route(
			self::NAMESPACE,
			'/account/sign-in',
			array(
				'methods'             => WP_REST_Server::CREATABLE,
				'callback'            => array( self::class, 'sign_in' ),
				'permission_callback' => array( self::class, 'can_use' ),
				'args'                => array(
					'email'    => array(
						'required'          => true,
						'type'              => 'string',
						'sanitize_callback' => 'sanitize_email',
					),
					'password' => array(
						'required' => true,
						'type'     => 'string',
					),
				),
			)
		);

		register_rest_route(
			self::NAMESPACE,
			'/account/sign-out',
			array(
				'methods'             => WP_REST_Server::CREATABLE,
				'callback'            => array( self::class, 'sign_out' ),
				'permission_callback' => array( self::class, 'can_use' ),
			)
		);

		register_rest_route(
			self::NAMESPACE,
			'/profile',
			array(
				array(
					'methods'             => WP_REST_Server::READABLE,
					'callback'            => array( self::class, 'get_profile' ),
					'permission_callback' => array( self::class, 'can_use' ),
				),
				array(
					'methods'             => WP_REST_Server::EDITABLE,
					'callback'            => array( self::class, 'put_profile' ),
					'permission_callback' => array( self::class, 'can_use' ),
				),
			)
		);

		register_rest_route(
			self::NAMESPACE,
			'/projects',
			array(
				'methods'             => WP_REST_Server::READABLE,
				'callback'            => array( self::class, 'projects' ),
				'permission_callback' => array( self::class, 'can_use' ),
			)
		);

		register_rest_route(
			self::NAMESPACE,
			'/marketplace/listings',
			array(
				'methods'             => WP_REST_Server::READABLE,
				'callback'            => array( self::class, 'listings' ),
				'permission_callback' => array( self::class, 'can_use' ),
			)
		);

		register_rest_route(
			self::NAMESPACE,
			'/pair',
			array(
				'methods'             => WP_REST_Server::CREATABLE,
				'callback'            => array( self::class, 'pair' ),
				'permission_callback' => array( self::class, 'can_administer' ),
				'args'                => array(
					'code' => array(
						'required'          => true,
						'type'              => 'string',
						'sanitize_callback' => 'sanitize_text_field',
					),
				),
			)
		);

		register_rest_route(
			self::NAMESPACE,
			'/diagnostics',
			array(
				'methods'             => WP_REST_Server::READABLE,
				'callback'            => array( self::class, 'diagnostics' ),
				'permission_callback' => array( self::class, 'can_administer' ),
			)
		);
	}

	public static function status(): WP_REST_Response {
		$user_id = get_current_user_id();
		return new WP_REST_Response(
			array(
				'configured'  => Settings::is_configured(),
				'mode'        => Settings::get( 'mode' ),
				'site_paired' => Settings::has_token(),
				'linked'      => Account::is_linked( $user_id ),
				'account_id'  => Account::account_id( $user_id ),
				'scopes'      => Account::scopes( $user_id ),
				'privacy'     => __(
					'This site sees only your profile and the projects you chose to synchronise. Your conversations, files and knowledge stay on your own machine.',
					'otwono-ai-connector'
				),
			)
		);
	}

	public static function register_account( WP_REST_Request $request ): WP_REST_Response|WP_Error {
		$result = Account::register(
			get_current_user_id(),
			(string) $request->get_param( 'email' ),
			(string) $request->get_param( 'password' )
		);
		if ( is_wp_error( $result ) ) {
			return $result;
		}
		// The verification token is a credential; it must not reach the browser.
		unset( $result['verification_token'] );
		return new WP_REST_Response( $result );
	}

	public static function sign_in( WP_REST_Request $request ): WP_REST_Response|WP_Error {
		$result = Account::sign_in(
			get_current_user_id(),
			(string) $request->get_param( 'email' ),
			(string) $request->get_param( 'password' ),
			array( 'profile.read', 'profile.write', 'projects.read', 'marketplace.read' )
		);
		if ( is_wp_error( $result ) ) {
			return $result;
		}
		return new WP_REST_Response( $result );
	}

	public static function sign_out(): WP_REST_Response {
		Account::sign_out( get_current_user_id() );
		return new WP_REST_Response( array( 'signed_out' => true ) );
	}

	public static function get_profile(): WP_REST_Response|WP_Error {
		$result = Account::profile( get_current_user_id() );
		return is_wp_error( $result ) ? $result : new WP_REST_Response( $result );
	}

	public static function put_profile( WP_REST_Request $request ): WP_REST_Response|WP_Error {
		$body    = $request->get_json_params();
		$profile = self::sanitise_profile( is_array( $body ) ? $body : array() );
		$result  = Account::save_profile( get_current_user_id(), $profile );
		return is_wp_error( $result ) ? $result : new WP_REST_Response( $result );
	}

	/** Only fields OTWONO defines survive, and each is sanitised for its type. */
	public static function sanitise_profile( array $input ): array {
		$strings = static fn( mixed $value ): array => is_array( $value )
			? array_values( array_slice( array_map( 'sanitize_text_field', array_filter( $value, 'is_string' ) ), 0, 40 ) )
			: array();

		$visibility = array();
		if ( isset( $input['visibility'] ) && is_array( $input['visibility'] ) ) {
			foreach ( $input['visibility'] as $field => $public ) {
				$field = sanitize_key( (string) $field );
				if ( in_array( $field, self::PROFILE_FIELDS, true ) ) {
					$visibility[ $field ] = (bool) $public;
				}
			}
		}

		$links = array();
		if ( isset( $input['portfolio_links'] ) && is_array( $input['portfolio_links'] ) ) {
			foreach ( array_slice( $input['portfolio_links'], 0, 20 ) as $link ) {
				$clean = esc_url_raw( (string) $link, array( 'http', 'https' ) );
				if ( '' !== $clean ) {
					$links[] = $clean;
				}
			}
		}

		return array(
			'display_name'    => sanitize_text_field( (string) ( $input['display_name'] ?? '' ) ),
			'biography'       => wp_kses_post( (string) ( $input['biography'] ?? '' ) ),
			'interests'       => $strings( $input['interests'] ?? array() ),
			'capabilities'    => $strings( $input['capabilities'] ?? array() ),
			'portfolio_links' => $links,
			'avatar_url'      => esc_url_raw( (string) ( $input['avatar_url'] ?? '' ), array( 'http', 'https' ) ) ?: null,
			'visibility'      => $visibility,
			'is_ai_identity'  => ! empty( $input['is_ai_identity'] ),
		);
	}

	public const PROFILE_FIELDS = array(
		'display_name',
		'biography',
		'interests',
		'capabilities',
		'portfolio_links',
		'avatar_url',
	);

	public static function projects(): WP_REST_Response|WP_Error {
		$result = Account::projects( get_current_user_id() );
		return is_wp_error( $result ) ? $result : new WP_REST_Response( $result );
	}

	public static function listings(): WP_REST_Response|WP_Error {
		$token = Account::token( get_current_user_id() );
		if ( '' === $token ) {
			return new WP_Error(
				'otwono_not_linked',
				__( 'Connect your OTWONO account first.', 'otwono-ai-connector' ),
				array( 'status' => 403 )
			);
		}
		$result = Client::get( '/v1/marketplace/listings', $token );
		return is_wp_error( $result ) ? $result : new WP_REST_Response( $result );
	}

	/** Redeem a pairing code shown in the desktop application. */
	public static function pair( WP_REST_Request $request ): WP_REST_Response|WP_Error {
		if ( ! Rate_Limiter::check( 'pair:' . Rate_Limiter::caller(), 10, HOUR_IN_SECONDS ) ) {
			return new WP_Error(
				'otwono_rate_limited',
				__( 'Too many attempts. Please try again later.', 'otwono-ai-connector' ),
				array( 'status' => 429 )
			);
		}

		$result = Client::post(
			'/v1/pairings/redeem',
			array(
				'code' => (string) $request->get_param( 'code' ),
				'site' => home_url(),
			)
		);
		if ( is_wp_error( $result ) ) {
			Logger::record( 'pairing_failed', array(), 'denied' );
			return $result;
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

		return new WP_REST_Response(
			array(
				'paired'  => true,
				'scopes'  => $result['scopes'] ?? array(),
				'message' => __( 'This site is now paired with your OTWONO account.', 'otwono-ai-connector' ),
			)
		);
	}

	public static function diagnostics(): WP_REST_Response {
		$health = Client::health();

		return new WP_REST_Response(
			array(
				'configured'   => Settings::is_configured(),
				'mode'         => Settings::get( 'mode' ),
				'base_url'     => Settings::base_url(),
				'site_paired'  => Settings::has_token(),
				'reachable'    => ! is_wp_error( $health ),
				'detail'       => is_wp_error( $health ) ? $health->get_error_message() : ( $health['stores'] ?? '' ),
				'php_version'  => PHP_VERSION,
				'wp_version'   => get_bloginfo( 'version' ),
				'plugin_version' => VERSION,
				'log'          => array_slice( Logger::entries(), -25 ),
			)
		);
	}
}
