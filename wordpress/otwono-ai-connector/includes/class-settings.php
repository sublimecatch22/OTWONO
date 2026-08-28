<?php
/**
 * Plugin settings, and the one place they are read and written.
 *
 * The relay token is deliberately kept in its own option, not in the settings
 * array, so that a settings export or a debug dump of the configuration cannot
 * carry it by accident.
 *
 * @package OTWONO\Connector
 */

declare( strict_types = 1 );

namespace OTWONO\Connector;

defined( 'ABSPATH' ) || exit;

final class Settings {

	/**
	 * Transport modes, matching the architecture document.
	 *
	 * - local:  the desktop application and this site are on the same machine
	 *           or network, and the site talks to it directly.
	 * - relay:  the site talks to a hosted OTWONO relay. The member's desktop
	 *           never becomes reachable from the internet.
	 */
	public const MODES = array( 'relay', 'local' );

	public static function defaults(): array {
		return array(
			'mode'               => 'relay',
			'relay_url'          => '',
			'local_url'          => 'http://127.0.0.1:8787',
			'allow_registration' => true,
			'delete_data_on_uninstall' => false,
			'account_id'         => '',
			'account_email'      => '',
			'scopes'             => array(),
			'paired_at'          => '',
		);
	}

	public static function all(): array {
		$stored = get_option( OPTION_KEY, array() );
		if ( ! is_array( $stored ) ) {
			$stored = array();
		}
		return array_merge( self::defaults(), $stored );
	}

	public static function get( string $key, mixed $fallback = null ): mixed {
		$all = self::all();
		return $all[ $key ] ?? $fallback;
	}

	/**
	 * Validate and store settings. Unknown keys are dropped rather than kept,
	 * so a crafted request cannot smuggle a value into the option.
	 */
	public static function save( array $input ): array {
		$clean = self::defaults();

		if ( isset( $input['mode'] ) && in_array( $input['mode'], self::MODES, true ) ) {
			$clean['mode'] = $input['mode'];
		}

		foreach ( array( 'relay_url', 'local_url' ) as $url_key ) {
			if ( isset( $input[ $url_key ] ) ) {
				$url = esc_url_raw( trim( (string) $input[ $url_key ] ) );
				$clean[ $url_key ] = self::is_acceptable_url( $url, $url_key ) ? untrailingslashit( $url ) : '';
			}
		}

		$existing = self::all();
		$clean['allow_registration'] = ! empty( $input['allow_registration'] );
		$clean['delete_data_on_uninstall'] = ! empty( $input['delete_data_on_uninstall'] );
		$clean['account_id']    = sanitize_text_field( (string) ( $input['account_id'] ?? $existing['account_id'] ) );
		$clean['account_email'] = sanitize_email( (string) ( $input['account_email'] ?? $existing['account_email'] ) );
		$clean['paired_at']     = sanitize_text_field( (string) ( $input['paired_at'] ?? $existing['paired_at'] ) );

		$scopes = $input['scopes'] ?? $existing['scopes'];
		$clean['scopes'] = is_array( $scopes ) ? array_values( array_map( 'sanitize_text_field', $scopes ) ) : array();

		update_option( OPTION_KEY, $clean, false );
		return $clean;
	}

	/**
	 * A relay address must be https and must not be a private or loopback host:
	 * a hosted site that could be pointed at an internal address is a
	 * server-side request forgery waiting to happen. The local mode is the
	 * documented exception, and is meant for a development machine.
	 */
	public static function is_acceptable_url( string $url, string $key = 'relay_url' ): bool {
		if ( '' === $url ) {
			return false;
		}
		$parts = wp_parse_url( $url );
		if ( empty( $parts['host'] ) || empty( $parts['scheme'] ) ) {
			return false;
		}
		if ( 'local_url' === $key ) {
			return in_array( $parts['scheme'], array( 'http', 'https' ), true );
		}
		if ( 'https' !== $parts['scheme'] ) {
			return false;
		}
		return ! self::is_private_host( (string) $parts['host'] );
	}

	public static function is_private_host( string $host ): bool {
		$host = strtolower( $host );
		if ( 'localhost' === $host || str_ends_with( $host, '.localhost' ) || str_ends_with( $host, '.local' ) ) {
			return true;
		}
		if ( filter_var( $host, FILTER_VALIDATE_IP ) ) {
			return ! filter_var(
				$host,
				FILTER_VALIDATE_IP,
				FILTER_FLAG_NO_PRIV_RANGE | FILTER_FLAG_NO_RES_RANGE
			);
		}
		return false;
	}

	/** The address the client should call, for the configured mode. */
	public static function base_url(): string {
		$settings = self::all();
		return 'local' === $settings['mode'] ? (string) $settings['local_url'] : (string) $settings['relay_url'];
	}

	public static function is_configured(): bool {
		return '' !== self::base_url();
	}

	/** The relay token. Stored on its own, never returned by `all()`. */
	public static function token(): string {
		return (string) get_option( TOKEN_KEY, '' );
	}

	public static function set_token( string $token ): void {
		if ( '' === $token ) {
			delete_option( TOKEN_KEY );
			return;
		}
		update_option( TOKEN_KEY, $token, false );
	}

	public static function has_token(): bool {
		return '' !== self::token();
	}
}
