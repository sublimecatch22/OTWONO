=== OTWONO AI Connector ===
Contributors: otwono
Tags: ai, agents, profile, marketplace
Requires at least: 6.4
Tested up to: 6.7
Requires PHP: 8.1
Stable tag: 0.1.0
License: Apache-2.0
License URI: https://www.apache.org/licenses/LICENSE-2.0

Connect a WordPress site to OTWONO AI so members can sign in, keep a profile,
and see the projects they chose to synchronise.

== Description ==

OTWONO AI is a local-first AI work platform. This plugin is the bridge between
a member's own copy of OTWONO and a WordPress site.

What the site receives:

* the member's OTWONO account link,
* the profile fields the member marked public,
* the titles and progress of projects the member switched synchronisation on for,
* marketplace listings the member is allowed to see.

What the site never receives:

* conversations or prompts,
* files, folders or anything indexed from them,
* AI models or anything about them,
* the member's OTWONO password.

The plugin talks to an OTWONO relay over https. It never dials a member's own
machine from the public internet.

== Installation ==

1. In WordPress, go to Plugins → Add New → Upload Plugin.
2. Choose `otwono-ai-connector.zip` and select Install Now, then Activate.
3. Go to Settings → OTWONO AI.
4. Choose Hosted relay and enter your relay address (https).
5. In the OTWONO desktop application, open Settings and choose "Show a pairing
   code". Enter that code on the settings screen within five minutes.
6. Add `[otwono_login]`, `[otwono_profile]` and `[otwono_dashboard]` to a page,
   or use the matching blocks in the editor.

== Frequently Asked Questions ==

= Does this send my conversations to the website? =

No. The relay has no table that could store one, and the plugin has no code
path that would send one.

= What happens if I delete the plugin? =

By default, your members' data is left alone and only the plugin's own settings
are removed. Deleting member data is a separate setting you have to switch on.

= Can the website reach the OTWONO app on my computer? =

Not in hosted mode. The site talks to the relay; the relay never connects back
to your machine. Local development mode exists for a site running on the same
machine as OTWONO, and is not suitable for a public site.

== Changelog ==

= 0.1.0 =
* First development release: account link, profile with per-field visibility,
  synchronised project list, marketplace browsing, pairing, diagnostics.
