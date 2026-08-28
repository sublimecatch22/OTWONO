<?php
/**
 * Fixed-window rate limiting, backed by transients.
 *
 * It exists so that a public endpoint — sign-in, registration, pairing —
 * cannot be hammered from one address.
 *
 * @package OTWONO\Connector
 */

declare( strict_types = 1 );

namespace OTWONO\Connector;

defined( 'ABSPATH' ) || exit;

final class Rate_Limiter {

	/**
	 * @return bool True when the caller is within the limit.
	 */
	public static function check( string $bucket, int $limit, int $window_seconds ): bool {
		$key   = 'otwono_rl_' . md5( $bucket );
		$count = (int) get_transient( $key );

		if ( $count >= $limit ) {
			return false;
		}

		set_transient( $key, $count + 1, $window_seconds );
		return true;
	}

	/**
	 * A coarse identifier for the caller. Only the first two octets are used,
	 * so the limiter works without recording where anyone is.
	 */
	public static function caller(): string {
		$address = '';
		if ( isset( $_SERVER['REMOTE_ADDR'] ) ) {
			$address = sanitize_text_field( wp_unslash( (string) $_SERVER['REMOTE_ADDR'] ) );
		}
		$parts = explode( '.', $address );
		if ( 4 === count( $parts ) ) {
			return $parts[0] . '.' . $parts[1] . '.x.x';
		}
		return 'unknown';
	}
}
