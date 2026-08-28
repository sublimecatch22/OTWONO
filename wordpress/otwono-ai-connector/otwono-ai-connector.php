<?php
/**
 * Plugin Name:       OTWONO AI Connector
 * Plugin URI:        https://otwono.com/
 * Description:       Connects a WordPress site to OTWONO AI. Members sign in, edit a profile, browse the human task marketplace, and see the projects they chose to synchronise. Prompts, files and knowledge never leave the member's own machine.
 * Version:           0.1.0
 * Requires at least: 6.4
 * Requires PHP:      8.1
 * Author:            OTWONO AI
 * License:           Apache-2.0
 * License URI:       https://www.apache.org/licenses/LICENSE-2.0
 * Text Domain:       otwono-ai-connector
 * Domain Path:       /languages
 *
 * @package OTWONO\Connector
 */

declare( strict_types = 1 );

namespace OTWONO\Connector;

// A plugin file loaded directly is either a misconfiguration or a probe.
defined( 'ABSPATH' ) || exit;

define( 'OTWONO_CONNECTOR_FILE', __FILE__ );
define( 'OTWONO_CONNECTOR_DIR', plugin_dir_path( __FILE__ ) );
define( 'OTWONO_CONNECTOR_URL', plugin_dir_url( __FILE__ ) );

require_once OTWONO_CONNECTOR_DIR . 'includes/constants.php';
require_once OTWONO_CONNECTOR_DIR . 'includes/class-settings.php';
require_once OTWONO_CONNECTOR_DIR . 'includes/class-logger.php';
require_once OTWONO_CONNECTOR_DIR . 'includes/class-rate-limiter.php';
require_once OTWONO_CONNECTOR_DIR . 'includes/class-client.php';
require_once OTWONO_CONNECTOR_DIR . 'includes/class-account.php';
require_once OTWONO_CONNECTOR_DIR . 'includes/class-rest.php';
require_once OTWONO_CONNECTOR_DIR . 'includes/class-shortcodes.php';
require_once OTWONO_CONNECTOR_DIR . 'includes/class-blocks.php';
require_once OTWONO_CONNECTOR_DIR . 'includes/class-installer.php';
require_once OTWONO_CONNECTOR_DIR . 'admin/class-admin.php';

/**
 * Wire the plugin up. Everything is a method on a class so nothing leaks into
 * the global namespace, and so each piece can be tested on its own.
 */
function bootstrap(): void {
	Installer::hooks();
	Admin::hooks();
	Rest::hooks();
	Shortcodes::hooks();
	Blocks::hooks();

	add_action( 'plugins_loaded', __NAMESPACE__ . '\\load_textdomain' );
	add_action( 'init', __NAMESPACE__ . '\\maybe_upgrade' );
}

function load_textdomain(): void {
	load_plugin_textdomain( 'otwono-ai-connector', false, dirname( plugin_basename( __FILE__ ) ) . '/languages' );
}

/**
 * Run migrations when the stored schema is older than this build.
 */
function maybe_upgrade(): void {
	$stored = (int) get_option( SCHEMA_KEY, 0 );
	if ( $stored < SCHEMA ) {
		Installer::migrate( $stored, SCHEMA );
		update_option( SCHEMA_KEY, SCHEMA, false );
	}
}

bootstrap();
