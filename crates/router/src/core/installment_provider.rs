//! An installment provider, asked from the eligibility call for the installment counts a card may
//! take, each with its total. Off unless `[installment_provider]` is configured.

use api_models::payment_methods::{
    PaymentMethodListInstallmentAmountDetails, PaymentMethodListInstallmentOption,
    PaymentMethodListInstallmentPlan,
};
use common_types::payments::{BillingFrequency, InstallmentInterestRate};
use common_utils::{
    id_type, pii,
    request::{Method, RequestBuilder, RequestContent},
    types::MinorUnit,
};
use external_services::http_client;
use hyperswitch_interfaces::types::Proxy;
use hyperswitch_masking::{Mask, PeekInterface, Secret};
use router_env::logger;
use rust_decimal::{prelude::ToPrimitive, Decimal};

use crate::{configs::settings::InstallmentProviderConfig, routes::SessionState};

/// How long the eligibility call waits for the provider.
const TIMEOUT_SECS: u64 = 2;

/// How long a quote is kept for confirm: as long as the provider holds it.
const KEEP_SECS: i64 = 30 * 60;

/// What the provider is sent: never the card number.
#[derive(Debug, serde::Serialize)]
struct QuoteRequest {
    hyperswitch_merchant_id: String,
    bin: Secret<String>,
    amount: i64,
    basket: Vec<BasketLine>,
}

#[derive(Debug, serde::Serialize)]
struct BasketLine {
    category: String,
    amount: i64,
}

/// The order's lines that name a category, each with its amount for all its quantity.
fn basket(order_details: Option<&[pii::SecretSerdeValue]>) -> Vec<BasketLine> {
    order_details
        .unwrap_or_default()
        .iter()
        .filter_map(|line| {
            let line = line.peek();
            Some(BasketLine {
                category: line["category"].as_str()?.to_string(),
                amount: line["amount"].as_i64()? * line["quantity"].as_i64().unwrap_or(1),
            })
        })
        .collect()
}

/// The provider's quote, kept for confirm to check the chosen count against.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Quote {
    pub id: String,
    /// The card's first 8 digits the quote is for
    pub bin: Secret<String>,
    pub amount: i64,
    pub counts: Vec<QuotedCount>,
}

/// A count the card may take, with what the customer pays for it in all and each month.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QuotedCount {
    pub count: std::num::NonZeroU8,
    pub total: i64,
    pub monthly: i64,
}

/// Asks the provider for the counts the card may take for the payment. `None` when the provider
/// is off, the BIN is not 8 digits, or the provider refuses, fails or does not answer in time.
pub async fn quote(
    proxy: &Proxy,
    provider: Option<&InstallmentProviderConfig>,
    payment_id: &id_type::PaymentId,
    hyperswitch_merchant_id: &id_type::MerchantId,
    card_bin: &cards::CardBin,
    amount: MinorUnit,
    order_details: Option<&[pii::SecretSerdeValue]>,
) -> Option<Quote> {
    let provider = provider?;
    let bin = card_bin.get_eight_digit_bin()?;
    let body = QuoteRequest {
        hyperswitch_merchant_id: hyperswitch_merchant_id.get_string_repr().to_string(),
        bin: Secret::new(bin.to_string()),
        amount: amount.get_amount_as_i64(),
        basket: basket(order_details),
    };
    let request = RequestBuilder::new()
        .method(Method::Post)
        .url(provider.url.as_str())
        .headers(vec![
            (
                http::header::AUTHORIZATION.to_string(),
                format!("Bearer {}", provider.token.peek()).into_masked(),
            ),
            (
                http::header::CONTENT_TYPE.to_string(),
                "application/json".to_string().into(),
            ),
        ])
        .set_body(RequestContent::Json(Box::new(body)))
        .build();
    let payment_id = payment_id.get_string_repr();
    let response = match http_client::send_request(proxy, request, Some(TIMEOUT_SECS)).await {
        Ok(response) => response,
        Err(_) => {
            logger::warn!(payment_id, "installment provider: no answer");
            return None;
        }
    };
    let status = response.status().as_u16();
    let quote = match status {
        201 => response.json::<Quote>().await.ok(),
        _ => None,
    };
    logger::info!(
        payment_id,
        status,
        quoted = quote.is_some(),
        "installment provider: answered"
    );
    quote
}

/// The quote's counts as the eligibility answer gives them.
pub fn options(
    quote: &Quote,
    currency: common_enums::Currency,
) -> Option<Vec<PaymentMethodListInstallmentOption>> {
    let amount = Decimal::from(quote.amount);
    (quote.amount > 0).then_some(())?;
    let available_plans = quote
        .counts
        .iter()
        .map(|quoted| {
            // The interest is shown as a rate; the total is what the customer pays.
            let rate = (Decimal::from(quoted.total) - amount) * Decimal::ONE_HUNDRED / amount;
            Some(PaymentMethodListInstallmentPlan {
                number_of_installments: quoted.count,
                billing_frequency: BillingFrequency::Month,
                interest_rate: InstallmentInterestRate::try_from(rate.round_dp(2).to_f64()?)
                    .ok()?,
                amount_details: PaymentMethodListInstallmentAmountDetails {
                    amount_per_installment: MinorUnit::new(quoted.monthly)
                        .to_major_unit_as_f64(currency)
                        .ok()?,
                    total_amount: MinorUnit::new(quoted.total)
                        .to_major_unit_as_f64(currency)
                        .ok()?,
                },
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(vec![PaymentMethodListInstallmentOption {
        payment_method: common_enums::PaymentMethod::Card,
        available_plans,
    }])
}

/// Where a payment's quote is kept.
pub fn key(merchant_id: &id_type::MerchantId, payment_id: &id_type::PaymentId) -> String {
    format!(
        "installment_quote:{}:{}",
        merchant_id.get_string_repr(),
        payment_id.get_string_repr()
    )
}

/// Keeps the quote on the payment for confirm, as long as the provider holds it.
async fn keep(
    state: &SessionState,
    merchant_id: &id_type::MerchantId,
    payment_id: &id_type::PaymentId,
    quote: &Quote,
) -> bool {
    let kept = match state.store.get_redis_conn() {
        Ok(redis) => redis
            .serialize_and_set_key_with_expiry(
                &key(merchant_id, payment_id).into(),
                quote,
                KEEP_SECS,
            )
            .await
            .is_ok(),
        Err(_) => false,
    };
    if !kept {
        logger::warn!(
            payment_id = payment_id.get_string_repr(),
            "installment provider: quote not kept"
        );
    }
    kept
}

/// The eligibility call's installment options: the provider's quote for the card, kept on the
/// payment for confirm. `None` leaves the payment to go as a single payment.
pub async fn eligibility(
    state: &SessionState,
    merchant_id: &id_type::MerchantId,
    payment_id: &id_type::PaymentId,
    card_bin: &cards::CardBin,
    amount: MinorUnit,
    currency: Option<common_enums::Currency>,
    order_details: Option<&[pii::SecretSerdeValue]>,
) -> Option<Vec<PaymentMethodListInstallmentOption>> {
    let quote = quote(
        &state.conf.proxy,
        state.conf.installment_provider.as_ref(),
        payment_id,
        merchant_id,
        card_bin,
        amount,
        order_details,
    )
    .await?;
    let options = options(&quote, currency?)?;
    keep(state, merchant_id, payment_id, &quote)
        .await
        .then_some(options)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use serde_json::{json, Value};
    use wiremock::{
        matchers::{body_json, header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

    const PATH: &str = "/internal/installments/quote";

    fn provider(server: &MockServer) -> InstallmentProviderConfig {
        InstallmentProviderConfig {
            url: format!("{}{PATH}", server.uri()).parse().unwrap(),
            token_file: Default::default(),
            token: Secret::new("provider-token".to_string()),
        }
    }

    /// Two lines: one in a category, one without.
    fn order_details() -> Vec<pii::SecretSerdeValue> {
        vec![
            Secret::new(json!({
                "product_name": "Kettle", "quantity": 2, "amount": 40000,
                "category": "electronics",
            })),
            Secret::new(json!({"product_name": "Gift wrap", "quantity": 1, "amount": 20000})),
        ]
    }

    fn answer() -> Value {
        json!({
            "id": "3b9f2c1e-5d4a-4e7b-8a6c-0d1e2f3a4b5c",
            "merchant": "7d1e5c3a-2b4f-4a6e-9c8d-1f2e3d4c5b6a",
            "bin": "40000150",
            "amount": 100000,
            "terminal_id": "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d",
            "expires_at": "2026-10-22T12:30:00Z",
            "counts": [
                {"count": 1, "total": 100000, "interest": 0, "monthly": 100000,
                 "payer": "merchant"},
                {"count": 3, "total": 100000, "interest": 0, "monthly": 33333,
                 "payer": "merchant"},
                {"count": 6, "total": 105000, "interest": 5000, "monthly": 17500,
                 "payer": "customer"},
            ],
        })
    }

    async fn ask(provider: Option<&InstallmentProviderConfig>, bin: &str) -> Option<Quote> {
        quote(
            &Proxy::default(),
            provider,
            &id_type::PaymentId::try_from(std::borrow::Cow::Borrowed("pay_1")).unwrap(),
            &id_type::MerchantId::try_from(std::borrow::Cow::Borrowed("merchant_1")).unwrap(),
            &bin.parse().unwrap(),
            MinorUnit::new(100000),
            Some(&order_details()),
        )
        .await
    }

    #[tokio::test]
    async fn an_eight_digit_bin_gets_the_quoted_options() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PATH))
            .and(header("authorization", "Bearer provider-token"))
            // The BIN, the amount, Hyperswitch's merchant and the basket's categorised lines:
            // never the card number.
            .and(body_json(json!({
                "hyperswitch_merchant_id": "merchant_1",
                "bin": "40000150",
                "amount": 100000,
                "basket": [{"category": "electronics", "amount": 80000}],
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(answer()))
            .expect(1)
            .mount(&server)
            .await;
        let quote = ask(Some(&provider(&server)), "40000150")
            .await
            .expect("quoted");
        assert_eq!(quote.id, "3b9f2c1e-5d4a-4e7b-8a6c-0d1e2f3a4b5c");
        assert_eq!(
            serde_json::to_value(&quote.counts).unwrap(),
            json!([
                {"count": 1, "total": 100000, "monthly": 100000},
                {"count": 3, "total": 100000, "monthly": 33333},
                {"count": 6, "total": 105000, "monthly": 17500},
            ])
        );
        let options = options(&quote, common_enums::Currency::TRY).expect("options");
        assert_eq!(
            serde_json::to_value(options).unwrap(),
            json!([{
                "payment_method": "card",
                "available_plans": [
                    {"number_of_installments": 1, "billing_frequency": "month",
                     "interest_rate": 0.0,
                     "amount_details": {"amount_per_installment": 1000.0, "total_amount": 1000.0}},
                    {"number_of_installments": 3, "billing_frequency": "month",
                     "interest_rate": 0.0,
                     "amount_details": {"amount_per_installment": 333.33, "total_amount": 1000.0}},
                    {"number_of_installments": 6, "billing_frequency": "month",
                     "interest_rate": 5.0,
                     "amount_details": {"amount_per_installment": 175.0, "total_amount": 1050.0}},
                ],
            }])
        );
    }

    #[tokio::test]
    async fn a_bin_of_other_than_eight_digits_asks_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(answer()))
            .expect(0)
            .mount(&server)
            .await;
        for bin in ["400001", "4000015", "400001501", "4000015012"] {
            assert_eq!(ask(Some(&provider(&server)), bin).await, None, "{bin}");
        }
    }

    #[tokio::test]
    async fn a_refusal_a_failure_or_a_timeout_leaves_none() {
        for answer in [
            ResponseTemplate::new(422).set_body_json(json!({"error": "no_route"})),
            ResponseTemplate::new(500),
            ResponseTemplate::new(201).set_body_json(json!({"id": "q"})),
            // Past the 2 s the eligibility call waits.
            ResponseTemplate::new(201)
                .set_body_json(answer())
                .set_delay(Duration::from_secs(3)),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(answer)
                .expect(1)
                .mount(&server)
                .await;
            let started = std::time::Instant::now();
            assert_eq!(ask(Some(&provider(&server)), "40000150").await, None);
            assert!(started.elapsed() < Duration::from_millis(2900));
        }
    }

    #[tokio::test]
    async fn with_the_provider_off_nothing_is_asked_and_the_answer_is_upstream_s() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(answer()))
            .expect(0)
            .mount(&server)
            .await;
        assert_eq!(ask(None, "40000150").await, None);
        let answer = api_models::payments::PaymentsEligibilityResponse {
            payment_id: id_type::PaymentId::try_from(std::borrow::Cow::Borrowed("pay_1")).unwrap(),
            sdk_next_action: api_models::payments::SdkNextAction {
                next_action: api_models::payments::NextActionCall::Confirm,
                should_block_confirm: None,
            },
            surcharge_details: None,
            amount_details: None,
            offer_details: None,
            installment_options: None,
        };
        assert_eq!(
            serde_json::to_string(&answer).unwrap(),
            r#"{"payment_id":"pay_1","sdk_next_action":{"next_action":"confirm","should_block_confirm":null},"surcharge_details":null}"#
        );
    }
}
