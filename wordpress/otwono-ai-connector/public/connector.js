/**
 * The connector's front-end behaviour.
 *
 * It talks only to this site's own REST routes — never to OTWONO directly —
 * so the member's token never reaches the browser. Every request carries the
 * WordPress REST nonce.
 */

(function () {
	'use strict';

	const config = window.otwonoConnector;
	if (!config) return;

	function request(path, options) {
		const settings = Object.assign(
			{
				headers: {
					'Content-Type': 'application/json',
					'X-WP-Nonce': config.nonce,
				},
				credentials: 'same-origin',
			},
			options || {}
		);
		return fetch('/wp-json/' + config.namespace + path, settings).then(async (response) => {
			const body = await response.json().catch(() => ({}));
			if (!response.ok) {
				throw new Error(body.message || config.strings.failed);
			}
			return body;
		});
	}

	function say(form, message, tone) {
		const target = form.querySelector('.otwono__message');
		if (!target) return;
		target.textContent = message;
		target.className = 'otwono__message otwono__message--' + (tone || 'info');
	}

	function busy(form, isBusy) {
		form.querySelectorAll('button').forEach((button) => {
			button.disabled = isBusy;
		});
	}

	document.querySelectorAll('[data-otwono-form="sign-in"]').forEach((form) => {
		form.addEventListener('submit', async (event) => {
			event.preventDefault();
			busy(form, true);
			say(form, '', 'info');
			try {
				const data = new FormData(form);
				await request('/account/sign-in', {
					method: 'POST',
					body: JSON.stringify({
						email: data.get('email'),
						password: data.get('password'),
					}),
				});
				window.location.reload();
			} catch (error) {
				say(form, error.message, 'error');
			} finally {
				busy(form, false);
			}
		});

		const registerButton = form.querySelector('[data-otwono-action="register"]');
		if (registerButton) {
			registerButton.addEventListener('click', async () => {
				busy(form, true);
				say(form, '', 'info');
				try {
					const data = new FormData(form);
					await request('/account/register', {
						method: 'POST',
						body: JSON.stringify({
							email: data.get('email'),
							password: data.get('password'),
						}),
					});
					say(
						form,
						'Account created. Check your email to verify it, then connect.',
						'success'
					);
				} catch (error) {
					say(form, error.message, 'error');
				} finally {
					busy(form, false);
				}
			});
		}
	});

	document.querySelectorAll('[data-otwono-action="sign-out"]').forEach((button) => {
		button.addEventListener('click', async () => {
			button.disabled = true;
			try {
				await request('/account/sign-out', { method: 'POST' });
				window.location.reload();
			} catch (error) {
				button.disabled = false;
				window.alert(error.message);
			}
		});
	});

	document.querySelectorAll('[data-otwono-form="profile"]').forEach((form) => {
		form.addEventListener('submit', async (event) => {
			event.preventDefault();
			busy(form, true);
			try {
				const data = new FormData(form);
				const interests = String(data.get('interests') || '')
					.split(',')
					.map((value) => value.trim())
					.filter(Boolean);

				await request('/profile', {
					method: 'PUT',
					body: JSON.stringify({
						display_name: data.get('display_name'),
						biography: data.get('biography'),
						interests: interests,
						is_ai_identity: data.get('is_ai_identity') === 'on',
						visibility: {
							display_name: data.get('visible_display_name') === 'on',
							biography: data.get('visible_biography') === 'on',
							interests: data.get('visible_interests') === 'on',
						},
					}),
				});
				say(form, config.strings.saved, 'success');
			} catch (error) {
				say(form, error.message, 'error');
			} finally {
				busy(form, false);
			}
		});
	});

	document.querySelectorAll('[data-otwono-region="listings"]').forEach(async (region) => {
		try {
			const body = await request('/marketplace/listings', { method: 'GET' });
			const listings = (body && body.listings) || [];
			if (listings.length === 0) {
				region.innerHTML =
					'<p class="otwono__muted">No tasks are published at the moment.</p>';
				return;
			}

			const list = document.createElement('ul');
			list.className = 'otwono__list';
			listings.forEach((listing) => {
				const item = document.createElement('li');

				const title = document.createElement('strong');
				title.textContent = listing.title || '';
				item.appendChild(title);

				const description = document.createElement('p');
				description.textContent = listing.description || '';
				item.appendChild(description);

				const pay = document.createElement('span');
				pay.className = 'otwono__muted';
				pay.textContent =
					'Simulated pay: ' +
					((listing.compensation_minor || 0) / 100).toFixed(2) +
					' ' +
					(listing.currency || 'USD') +
					' — no money moves.';
				item.appendChild(pay);

				list.appendChild(item);
			});

			region.textContent = '';
			region.appendChild(list);
		} catch (error) {
			region.textContent = error.message;
		}
	});
})();
