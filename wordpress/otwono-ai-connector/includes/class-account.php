<?php
/**
 * The link between a WordPress user and an OTWONO account.
 *
 * A member's own relay token is stored in their user meta, not in a site-wide
 * option, so one member's session can never act as another's. Nothing here
 * grants a WordPress capability beyond the connector's own.
 *
 * @package OTWONO\Connector
 */

declare( strict_types = 1 );

namespace OTWONO\Connector;

use WP_Error;
use WP_User;

defined( 'ABSPATH' ) || exit;

final class Account {

	private const TOKEN_META   = '_otwono_token';
	private const ACCOUNT_META = '_otwono_account_id';
	private const SCOPES_META  = '_otwono_scopes';

	/** Link the current user to an OTWONO account. */
	public static function store( int $user_id, string $account_id, string $token, array $scopes ): void {
		update_user_meta( $user_id, self::ACCOUNT_META, sanitize_text_field( $account_id ) );
		update_user_meta( $user_id, self::TOKEN_META, $token );
		update_user_meta(
			$user_id,
			self::SCOPES_META,
			array_values( array_map( 'sanitize_text_field', $scopes ) )
		);
		Logger::record( 'account_linked', array( 'user' => $user_id ) );
	}

	public static function forget( int $user_id ): void {
		delete_user_meta( $user_id, self::TOKEN_META );
		delete_user_meta( $user_id, self::ACCOUNT_META );
		delete_user_meta( $user_id, self::SCOPES_META );
		Logger::record( 'account_unlinked', array( 'user' => $user_id ) );
	}

	public static function token( int $user_id ): string {
		$token = get_user_meta( $user_id, self::TOKEN_META, true );
		return is_string( $token ) ? $token : '';
	}

	public static function account_id( int $user_id ): string {
		$id = get_user_meta( $user_id, self::ACCOUNT_META, true );
		return is_string( $id ) ? $id : '';
	}

	public static function scopes( int $user_id ): array {
		$scopes = get_user_meta( $user_id, self::SCOPES_META, true );
		return is_array( $scopes ) ? $scopes : array();
	}

	public static function is_linked( int $user_id ): bool {
		return '' !== self::token( $user_id );
	}

	/**
	 * Register a new OTWONO account for the signed-in WordPress user.
	 */
	public static function register( int $user_id, string $email, string $password ): array|WP_Error {
		if ( ! Settings::get( 'allow_registration', true ) ) {
			return new WP_Error(
				'otwono_registration_closed',
				__( 'Registration is closed on this site.', 'otwono-ai-connector' )
			);
		}
		if ( ! Rate_Limiter::check( 'register:' . Rate_Limiter::caller(), 5, HOUR_IN_SECONDS ) ) {
			return new WP_Error(
				'otwono_rate_limited',
				__( 'Too many attempts. Please try again later.', 'otwono-ai-connector' )
			);
		}

		$user = get_userdata( $user_id );
		$name = $user instanceof WP_User ? $user->display_name : '';

		return Client::post(
			'/v1/accounts',
			array(
				'email'        => $email,
				'password'     => $password,
				'display_name' => $name,
			)
		);
	}

	/**
	 * Sign in and keep the resulting token against the WordPress user.
	 */
	public static function sign_in( int $user_id, string $email, string $password, array $scopes ): array|WP_Error {
		if ( ! Rate_Limiter::check( 'signin:' . Rate_Limiter::caller(), 10, 15 * MINUTE_IN_SECONDS ) ) {
			return new WP_Error(
				'otwono_rate_limited',
				__( 'Too many sign-in attempts. Please try again later.', 'otwono-ai-connector' )
			);
		}

		$result = Client::post(
			'/v1/accounts/sign-in',
			array(
				'email'        => $email,
				'password'     => $password,
				'device_label' => wp_parse_url( home_url(), PHP_URL_HOST ) ?? 'WordPress',
				'scopes'       => $scopes,
			)
		);

		if ( is_wp_error( $result ) ) {
			Logger::record( 'signin_failed', array( 'user' => $user_id ), 'denied' );
			return $result;
		}

		self::store(
			$user_id,
			(string) ( $result['account_id'] ?? '' ),
			(string) ( $result['token'] ?? '' ),
			is_array( $result['scopes'] ?? null ) ? $result['scopes'] : $scopes
		);

		// The token is never returned to the browser.
		unset( $result['token'] );
		return $result;
	}

	/** Sign out on the relay, then forget the token locally either way. */
	public static function sign_out( int $user_id ): void {
		$token = self::token( $user_id );
		if ( '' !== $token ) {
			Client::post( '/v1/accounts/sign-out', array(), $token );
		}
		self::forget( $user_id );
	}

	public static function profile( int $user_id ): array|WP_Error {
		$token = self::token( $user_id );
		if ( '' === $token ) {
			return new WP_Error(
				'otwono_not_linked',
				__( 'Connect your OTWONO account first.', 'otwono-ai-connector' )
			);
		}
		return Client::get( '/v1/profile', $token );
	}

	public static function save_profile( int $user_id, array $profile ): array|WP_Error {
		$token = self::token( $user_id );
		if ( '' === $token ) {
			return new WP_Error(
				'otwono_not_linked',
				__( 'Connect your OTWONO account first.', 'otwono-ai-connector' )
			);
		}
		return Client::put( '/v1/profile', $profile, $token );
	}

	public static function projects( int $user_id ): array|WP_Error {
		$token = self::token( $user_id );
		if ( '' === $token ) {
			return new WP_Error(
				'otwono_not_linked',
				__( 'Connect your OTWONO account first.', 'otwono-ai-connector' )
			);
		}
		return Client::get( '/v1/projects', $token );
	}
}
