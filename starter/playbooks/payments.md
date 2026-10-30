# Product with payments

Switches: database, sign-in, payments (Stripe test mode first).

**Builds first:** the landing page, sign-in, checkout in test mode, then the product logic behind
it. Prices live on the server; the browser never sets an amount.

**Asks (one card at most):** the price and what it buys, if the request doesn't say.

**Never asks:** for a Stripe key (it arrives through Lightbulb), the checkout layout, receipts
wording. Live payments only after the owner says so.

**Done when:** a test checkout completes in the preview and the purchase shows in the app.
