<?php
/**
 * Constants shared by the plugin and by `uninstall.php`.
 *
 * They live here rather than in the main plugin file because WordPress runs
 * `uninstall.php` on its own, without loading the plugin, and it still needs
 * to know which options to remove.
 *
 * @package OTWONO\Connector
 */

declare( strict_types = 1 );

namespace OTWONO\Connector;

defined( 'ABSPATH' ) || exit;

const VERSION    = '0.1.0';
const OPTION_KEY = 'otwono_connector_settings';
const TOKEN_KEY  = 'otwono_connector_token';
const SCHEMA_KEY = 'otwono_connector_schema';
const SCHEMA     = 2;
const USER_META  = 'otwono_account';
const CAPABILITY = 'otwono_use_connector';
