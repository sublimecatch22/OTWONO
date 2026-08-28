# The WordPress plugin

**OTWONO AI Connector.** Lets members of your WordPress site sign in with an
OTWONO account, keep a profile, and see the project metadata you chose to
publish.

| | |
|---|---|
| Requires | WordPress 6.4+, PHP 8.1+ |
| Version | 0.1.0 |
| Talks to | The relay. **Never to anybody's desktop machine.** |

---

## 1. How the pieces relate

```
Your desktop  ──push, when you press the button──▶  Relay  ◀──read──  WordPress
```

The plugin has no code path that dials a private address, and the settings
screen refuses a relay URL that is not `https` or that resolves to a private or
loopback host. A visitor to your site can never reach your machine.

## 2. Before you start

You need a relay. Running one is covered in
[ADMIN_GUIDE.md](ADMIN_GUIDE.md#3-running-the-relay). Without one there is
nothing for the plugin to talk to, and it will say so rather than pretending.

## 3. Installing

1. Take `otwono-ai-connector.zip` from the release folder.
2. WordPress admin → **Plugins → Add New → Upload Plugin** → choose the ZIP →
   *Install Now* → *Activate*.

Activating adds an `otwono_use_connector` capability to the roles that should
have it. Nobody without it can use any of the plugin's endpoints.

The ZIP contains no build artefacts — the blocks are server-rendered, so there
is no compiled JavaScript that could go stale.

## 4. Pairing the site

The site itself needs a token, obtained once with a code from the desktop
application.

1. **In OTWONO:** Settings → **Show a pairing code**. It is shown once.
2. **In WordPress:** Settings → OTWONO AI → paste the code → **Pair**.

The code is **single use** and short lived. Using it twice fails; mint a fresh
one. Only its hash was ever stored, on either side.

The paired site gets read-only scopes. It can read what members published; it
**cannot** write project metadata into anyone's account — that needs
`projects.write`, which a site never asks for.

## 5. Putting it on pages

Five shortcodes, and a block for each with the same renderer, so a page looks
the same however it was built.

| Shortcode | Block | Shows |
|---|---|---|
| `[otwono_status]` | OTWONO connection status | Whether the site is paired and the relay reachable. Useful while setting up; take it off public pages afterwards. |
| `[otwono_login]` | OTWONO sign-in | Sign in or out with an OTWONO account. |
| `[otwono_profile]` | OTWONO profile | The signed-in member's profile, editable. |
| `[otwono_dashboard]` | OTWONO dashboard | Their synchronised project metadata. |
| `[otwono_marketplace]` | OTWONO marketplace | Open listings, with the simulated-payment notice. |

A typical member page:

```
[otwono_login]
[otwono_profile]
[otwono_dashboard]
```

## 6. What a member sees

**Signing in** exchanges their OTWONO email and password with the relay for a
token, stored in **their own user meta**. It is never sent to the browser and
never shared between members.

**Their profile** is private by default, **field by field**. Nothing appears
publicly until they mark that field public. A profile that represents an AI
carries an unmissable notice wherever it is shown.

**Their dashboard** shows the projects they synchronised: title, state, task
counts. There is no field that could carry an objective, a task instruction or
an output — the relay has nowhere to put one.

## 7. The REST endpoints

Under `otwono/v1`. Every one checks the capability and a nonce.

| | |
|---|---|
| `GET /status` | Is the site paired, is the relay reachable. |
| `POST /account/register`, `/account/sign-in`, `/account/sign-out` | The member's session. |
| `GET`/`POST /profile` | Read and update the member's profile. |
| `GET /projects` | Their synchronised project metadata. |
| `GET /marketplace/listings` | Open listings. |
| `POST /pair` | Redeem a pairing code. Administrator only, rate limited. |
| `GET /diagnostics` | What an administrator needs to see what is wrong. |

## 8. Security notes

- Capability checks **and** nonces on every action.
- Sanitise on the way in, escape on the way out. No user text is ever echoed
  raw.
- Rate limits on sign-in, registration and pairing.
- The site token is in its own option, not mixed into the settings array; a
  plugin upgrade migrates an older layout automatically.
- The relay URL must be `https` and must not be private or loopback.
- No member token ever reaches the browser.

## 9. Uninstalling

**Member data is kept by default.** Deleting other people's accounts and
profiles is a decision for a person, not a side effect of clicking uninstall.
The plugin's own settings and site token are removed.

To remove member data as well, use the explicit option on the settings screen
before uninstalling.

## 10. Testing it

The plugin has its own suites, and neither needs a WordPress installation:

```bash
php wordpress/tests/run-tests.php          # 28 tests, outbound HTTP stubbed
./scripts/run-wordpress-live-tests.sh      # against a relay that is really running
```

The second one starts the relay binary against a throwaway database and drives
the plugin's own code over real HTTP: pairing with a single-use code, member
sign-in, editing a profile and reading back only the fields marked public, and
seeing synchronised project metadata with no content in it.

## 11. When something is wrong

| Symptom | Cause |
|---|---|
| "This site is not paired" | No token. Mint a code and pair. |
| The pairing code is refused | Single use, and short lived. Mint a fresh one. |
| The relay URL will not save | It must be `https` and not a private or loopback host. |
| A member cannot sign in | Wrong password, or the relay is unreachable. `GET /diagnostics` distinguishes them. |
| A dashboard is empty | Nothing has been synchronised. Marking a project is not sending it — press *Send project metadata* in OTWONO. |
| A profile shows nothing | Every field is private until the member publishes it. |
