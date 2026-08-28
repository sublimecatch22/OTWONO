<?php
/**
 * Editor blocks.
 *
 * Each block is registered server-side with a render callback that calls the
 * same renderer as its shortcode. There is no compiled JavaScript in the
 * plugin, so the ZIP can never ship a stale build, and the block and the
 * shortcode can never disagree about what they show.
 *
 * @package OTWONO\Connector
 */

declare( strict_types = 1 );

namespace OTWONO\Connector;

defined( 'ABSPATH' ) || exit;

final class Blocks {

	/** Block name => the renderer that produces its markup. */
	private const BLOCKS = array(
		'otwono/status'      => array( Shortcodes::class, 'status' ),
		'otwono/login'       => array( Shortcodes::class, 'login' ),
		'otwono/profile'     => array( Shortcodes::class, 'profile' ),
		'otwono/dashboard'   => array( Shortcodes::class, 'dashboard' ),
		'otwono/marketplace' => array( Shortcodes::class, 'marketplace' ),
	);

	private const TITLES = array(
		'otwono/status'      => 'OTWONO connection status',
		'otwono/login'       => 'OTWONO sign-in',
		'otwono/profile'     => 'OTWONO profile',
		'otwono/dashboard'   => 'OTWONO dashboard',
		'otwono/marketplace' => 'OTWONO marketplace',
	);

	public static function hooks(): void {
		add_action( 'init', array( self::class, 'register' ) );
	}

	public static function register(): void {
		if ( ! function_exists( 'register_block_type' ) ) {
			return;
		}

		foreach ( self::BLOCKS as $name => $renderer ) {
			register_block_type(
				$name,
				array(
					'api_version'     => 3,
					'title'           => self::TITLES[ $name ] ?? $name,
					'category'        => 'widgets',
					'icon'            => 'admin-network',
					'description'     => __(
						'A part of the OTWONO AI connector.',
						'otwono-ai-connector'
					),
					'supports'        => array( 'html' => false ),
					'render_callback' => static function () use ( $renderer ): string {
						return (string) call_user_func( $renderer );
					},
				)
			);
		}
	}

	/** Exposed for tests: the blocks this plugin registers. */
	public static function names(): array {
		return array_keys( self::BLOCKS );
	}
}
