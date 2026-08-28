<?php
/**
 * Activation, migration, deactivation and uninstall.
 *
 * Uninstalling keeps the member's data by default. Deleting it is a separate,
 * explicit setting, because a plugin that quietly destroys data on removal is
 * a plugin nobody can safely try.
 *
 * @package OTWONO\Connector
 */

declare( strict_types = 1 );

namespace OTWONO\Connector;

defined( 'ABSPATH' ) || exit;

final class Installer {

	public static function hooks(): void {
		register_activation_hook( OTWONO_CONNECTOR_FILE, array( self::class, 'activate' ) );
		register_deactivation_hook( OTWONO_CONNECTOR_FILE, array( self::class, 'deactivate' ) );
	}

	public static function activate(): void {
		self::add_capabilities();

		if ( false === get_option( OPTION_KEY, false ) ) {
			add_option( OPTION_KEY, Settings::defaults(), '', false );
		}
		update_option( SCHEMA_KEY, SCHEMA, false );

		Logger::record( 'plugin_activated', array( 'version' => VERSION ) );
	}

	public static function deactivate(): void {
		// Capabilities are removed so a deactivated plugin grants nothing, but
		// no member data is touched.
		self::remove_capabilities();
		Logger::record( 'plugin_deactivated', array() );
	}

	/**
	 * Give the connector's own capability to roles that should have it.
	 *
	 * Deliberately narrow: this capability lets a member use OTWONO on the
	 * site. It never grants `manage_options` or any other administrative
	 * right, and it is never given to a subscriber automatically without the
	 * site owner's roles saying so.
	 */
	public static function add_capabilities(): void {
		foreach ( array( 'administrator', 'editor', 'author', 'contributor', 'subscriber' ) as $role_name ) {
			$role = get_role( $role_name );
			if ( $role instanceof \WP_Role ) {
				$role->add_cap( CAPABILITY );
			}
		}
	}

	public static function remove_capabilities(): void {
		foreach ( wp_roles()->get_names() as $role_name => $label ) {
			unset( $label );
			$role = get_role( $role_name );
			if ( $role instanceof \WP_Role ) {
				$role->remove_cap( CAPABILITY );
			}
		}
	}

	/**
	 * Migrate stored data forward. Each step is idempotent so a partly
	 * completed upgrade can simply be run again.
	 */
	public static function migrate( int $from, int $to ): void {
		if ( $from === $to ) {
			return;
		}

		if ( $from < 1 ) {
			// 1: the settings option gains its defaults.
			$existing = get_option( OPTION_KEY, array() );
			update_option(
				OPTION_KEY,
				array_merge( Settings::defaults(), is_array( $existing ) ? $existing : array() ),
				false
			);
		}

		if ( $from < 2 ) {
			// 2: the relay token moves out of the settings array into its own
			// option, so a settings export cannot carry it.
			$existing = get_option( OPTION_KEY, array() );
			if ( is_array( $existing ) && ! empty( $existing['token'] ) ) {
				Settings::set_token( (string) $existing['token'] );
				unset( $existing['token'] );
				update_option( OPTION_KEY, $existing, false );
			}
		}

		Logger::record( 'plugin_migrated', array( 'from' => $from, 'to' => $to ) );
	}

	/**
	 * Called by uninstall.php. Removes the plugin's own settings always, and
	 * member data only when the site owner asked for that.
	 */
	public static function uninstall(): void {
		$delete_everything = (bool) Settings::get( 'delete_data_on_uninstall', false );

		delete_option( OPTION_KEY );
		delete_option( TOKEN_KEY );
		delete_option( SCHEMA_KEY );
		self::remove_capabilities();

		if ( $delete_everything ) {
			Logger::clear();
			$users = get_users( array( 'fields' => 'ID' ) );
			foreach ( $users as $user_id ) {
				Account::forget( (int) $user_id );
			}
		}
	}
}
