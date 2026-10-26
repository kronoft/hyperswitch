# feat(installments): an installment provider asked from the eligibility call

From branch `s6b-upstream`: `s6b-installments` without its arm64 Dockerfile change
(`build: link with lld for arm64`), which is not part of this pull request. Built on v1.127.0;
it will be rebased on `main` when opened.

## What

An optional installment provider. When `[installment_provider]` is configured:

- **The payment method list** (`GET /payments/{payment_id}/client`) answers
  `card_bin_eligibility: true` and asks the SDK for an eligibility check, so the SDK sends the
  card's first 8 digits as `card_bin` to `POST /payments/{payment_id}/eligibility`.
- **The eligibility call**, given an 8-digit `card_bin`, posts
  `{hyperswitch_merchant_id, payment_id, bin, amount, basket}` to the provider with a bearer token
  and a 2-second timeout. The basket is the order's categorised lines. A `201` quote, each count
  with its total and monthly amount, is answered as `installment_options` (the existing
  `PaymentMethodListInstallmentOption` shape) and kept in Redis under the payment for 30 minutes.
  A refusal, a failure or a timeout leaves the answer without options.
- **Confirm** holds `installment_data` to the kept quote:
  - the card's first 8 digits must be the quote's BIN;
  - the count must be one the quote offers;
  - the amount must be the one quoted.

  The interest is the quoted total less the amount, so the connector is charged exactly the
  quoted total, once, also on a retried confirm. Anything else is refused before the connector
  with `InvalidRequestData`, saying why. Offers and surcharges are not combined with a quote.
  Without `installment_data`, or with a count of 1, the payment is a single payment and always
  allowed. Without a kept quote, a merchant's own `installment_options` take the existing EMI path
  unchanged.
- **The `checkout` connector** adds `installments` (the count, when above 1) to its request
  `metadata`, unless the merchant's metadata already has the key.

## Why

In some markets, Turkey among them, installments are priced per card, issuer and merchant by the
acquiring side. The customer should see the counts their card may take, and what each costs,
while typing the card number and before confirming. The existing `installment_options` are set
per payment by the merchant, before the card is known.

## Off by default

With no `[installment_provider]` section, nothing changes:

- the provider is never called;
- eligibility answers, payment method lists and confirms are byte-identical to today's
  (`installment_options` and `card_bin_eligibility` are omitted when absent);
- the tests below show both.

The provider never receives the card number, only the 8-digit BIN. The BIN, the token and the
card number are never logged.

```toml
[installment_provider]
url = "https://installments.example.com/quote"
token_file = "/run/secrets/installment_provider"
```

## Tests

`cargo test -p router --lib installment_provider`, with the provider mocked by `wiremock`:

- `an_eight_digit_bin_gets_the_quoted_options`
- `a_bin_of_other_than_eight_digits_asks_nothing`
- `a_refusal_a_failure_or_a_timeout_leaves_none`
- `with_the_provider_off_nothing_is_asked_and_the_answer_is_upstream_s`
- `the_list_turns_the_bin_call_on_only_with_the_provider`
- `a_quoted_count_reaches_the_connector_at_exactly_its_total` (a customer-paid and a merchant-paid
  count)
- `another_count_card_or_amount_is_refused` (another count, another card, a token with no
  readable BIN, an offer, a surcharge, another amount)
- `without_a_quote_the_merchant_s_options_keep_upstream_s_path`

`cargo test -p hyperswitch_connectors --lib checkout::transformers::tests`:

- `a_chosen_count_above_one_reaches_the_metadata`
- `a_single_payment_or_the_merchant_s_own_count_is_left_alone`

End to end, a contract suite outside this repository runs the router image against a provider
and a connector: eligibility with a BIN, confirm with 3 and with a customer-paid 6, and the
connector charged the quoted total. All test data is synthetic.

The web SDK's side is a separate pull request on `hyperswitch-web`.
