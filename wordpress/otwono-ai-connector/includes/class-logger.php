<?php
/**
 * Audit-friendly logging.
 *
 * Entries record what happened and who did it. Values that could carry a
 * secret are removed before the entry is stored, not when it is displayed, so
 * a database dump never contains one.
 *
 * @package OTWONO\Connector
 */

declare( strict_types = 1 );

namespace OTWONO\Connector;

defined( 'ABSPATH' ) || exit;

final class Logger {

	private const OPTION = 'otwono_connector_log';
	private const LIMIT  = 300;

	/** Keys whose values are replaced before storage. */
	private const REDACT_EXACT = array(
		'token', 'secret', 'password', 'passphrase', 'credential', 'credentials',
		'auth', 'authorization', 'bearer', 'cookie', 'key', 'jwt', 'code',
	);

	private const REDACT_FRAGMENTS = array(
		'apikey', 'accesstoken', 'refreshtoken', 'idtoken', 'authtoken',
		'bearertoken', 'sessiontoken', 'privatekey', 'secretkey', 'clientsecret',
	);

	public const REDACTED = '[redacted]';

	public static function record( string $action, array $detail = array(), string $outcome = 'ok' ): void {
		$entries   = self::entries();
		$entries[] = array(
			'at'      => gmdate( 'c' ),
			'action'  => sanitize_key( $action ),
			'outcome' => sanitize_key( $outcome ),
			'user'    => get_current_user_id(),
			'detail'  => self::redact( $detail ),
		);

		if ( count( $entries ) > self::LIMIT ) {
			$entries = array_slice( $entries, - self::LIMIT );
		}
		update_option( self::OPTION, $entries, false );
	}

	public static function entries(): array {
		$stored = get_option( self::OPTION, array() );
		return is_array( $stored ) ? $stored : array();
	}

	public static function clear(): void {
		delete_option( self::OPTION );
	}

	/**
	 * Replace sensitive values anywhere in a structure, at any depth.
	 */
	public static function redact( array $data ): array {
		$out = array();
		foreach ( $data as $key => $value ) {
			if ( is_string( $key ) && self::is_sensitive( $key ) ) {
				$out[ $key ] = self::REDACTED;
				continue;
			}
			if ( is_array( $value ) ) {
				$out[ $key ] = self::redact( $value );
				continue;
			}
			$out[ $key ] = is_scalar( $value ) || null === $value ? $value : '[object]';
		}
		return $out;
	}

	public static function is_sensitive( string $key ): bool {
		$normalised = preg_replace( '/[^a-z0-9]/', '', strtolower( $key ) ) ?? '';
		if ( in_array( $normalised, self::REDACT_EXACT, true ) ) {
			return true;
		}
		foreach ( self::REDACT_FRAGMENTS as $fragment ) {
			if ( str_contains( $normalised, $fragment ) ) {
				return true;
			}
		}
		return false;
	}
}
