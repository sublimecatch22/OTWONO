<?php
/**
 * Uninstall.
 *
 * WordPress runs this file when the plugin is deleted. By default it removes
 * the plugin's own configuration and leaves every member's data alone; the
 * site owner can opt into a full deletion in the settings.
 *
 * @package OTWONO\Connector
 */

declare( strict_types = 1 );

defined( 'WP_UNINSTALL_PLUGIN' ) || exit;

require_once plugin_dir_path( __FILE__ ) . 'includes/constants.php';
require_once plugin_dir_path( __FILE__ ) . 'includes/class-settings.php';
require_once plugin_dir_path( __FILE__ ) . 'includes/class-logger.php';
require_once plugin_dir_path( __FILE__ ) . 'includes/class-account.php';
require_once plugin_dir_path( __FILE__ ) . 'includes/class-installer.php';

OTWONO\Connector\Installer::uninstall();
