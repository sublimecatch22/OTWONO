<?php
/**
 * The HTTP client for the OTWONO relay.
 *
 * Every outbound request goes through here, so the token is attached in one
 * place, timeouts are consistent, and no other file needs to know the token
 * exists.
 *
 * @package OTWONO\Connector
 */

declare( strict_types = 1 );

namespace OTWONO\Connector;

use WP_Error;

defined( 'ABSPATH' ) || exit;

final class Client {

	private const TIMEOUT = 15;

	/**
	 * @return array|WP_Error Decoded body on success.
	 */
	public static function request(
		string $method,
		string $path,
		array $body = array(),
		?string $token = null
	): array|WP_Error {
		$base = Settings::base_url();
		if ( '' === $base ) {
			return new WP_Error(
				'otwono_not_configured',
				__( 'OTWONO is not connected yet. An administrator needs to finish setup.', 'otwono-ai-connector' )
			);
		}

		$url  = $base . '/' . ltrim( $path, '/' );
		$args = array(
			'method'      => $method,
			'timeout'     => self::TIMEOUT,
			'redirection' => 0,
			'headers'     => array(
				'Content-Type' => 'application/json',
				'Accept'       => 'application/json',
				'User-Agent'   => 'OTWONO-AI-Connector/' . VERSION,
			),
		);

		$bearer = $token ?? Settings::token();
		if ( '' !== $bearer ) {
			$args['headers']['Authorization'] = 'Bearer ' . $bearer;
		}
		if ( array() !== $body || in_array( $method, array( 'POST', 'PUT', 'PATCH' ), true ) ) {
			$args['body'] = wp_json_encode( $body );
		}

		$response = wp_remote_request( $url, $args );

		if ( is_wp_error( $response ) ) {
			Logger::record( 'relay_request_failed', array( 'path' => $path ), 'failed' );
			return new WP_Error(
				'otwono_unreachable',
				sprintf(
					/* translators: %s: the underlying error message. */
					__( 'OTWONO could not be reached: %s', 'otwono-ai-connector' ),
					$response->get_error_message()
				)
			);
		}

		$status  = (int) wp_remote_retrieve_response_code( $response );
		$decoded = json_decode( (string) wp_remote_retrieve_body( $response ), true );
		$decoded = is_array( $decoded ) ? $decoded : array();

		if ( $status >= 400 ) {
			$message = $decoded['error']['message']
				?? __( 'OTWONO refused that request.', 'otwono-ai-connector' );
			$code    = $decoded['error']['code'] ?? 'otwono_error';
			Logger::record( 'relay_request_refused', array( 'path' => $path, 'status' => $status ), 'denied' );
			return new WP_Error( sanitize_key( (string) $code ), (string) $message, array( 'status' => $status ) );
		}

		return $decoded;
	}

	public static function get( string $path, ?string $token = null ): array|WP_Error {
		return self::request( 'GET', $path, array(), $token );
	}

	public static function post( string $path, array $body = array(), ?string $token = null ): array|WP_Error {
		return self::request( 'POST', $path, $body, $token );
	}

	public static function put( string $path, array $body = array(), ?string $token = null ): array|WP_Error {
		return self::request( 'PUT', $path, $body, $token );
	}

	/** Reachability, for the diagnostics screen. */
	public static function health(): array|WP_Error {
		return self::get( '/health' );
	}
}
