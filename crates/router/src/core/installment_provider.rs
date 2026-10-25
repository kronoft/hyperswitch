//! An installment provider, asked from the eligibility call for the installment counts a card may
//! take, each with its total. Off unless `[installment_provider]` is configured.

use api_models::payment_methods::PaymentMethodListInstallmentOption;
use common_utils::{id_type, pii, types::MinorUnit};
use hyperswitch_interfaces::types::Proxy;
use hyperswitch_masking::Secret;

use crate::{configs::settings::InstallmentProviderConfig, routes::SessionState};

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
    _proxy: &Proxy,
    _provider: Option<&InstallmentProviderConfig>,
    _payment_id: &id_type::PaymentId,
    _hyperswitch_merchant_id: &id_type::MerchantId,
    _card_bin: &cards::CardBin,
    _amount: MinorUnit,
    _order_details: Option<&[pii::SecretSerdeValue]>,
) -> Option<Quote> {
    None
}

/// The quote's counts as the eligibility answer gives them.
pub fn options(
    _quote: &Quote,
    _currency: common_enums::Currency,
) -> Option<Vec<PaymentMethodListInstallmentOption>> {
    None
}

/// Keeps the quote on the payment for confirm, as long as the provider holds it.
pub async fn keep(
    _state: &SessionState,
    _merchant_id: &id_type::MerchantId,
    _payment_id: &id_type::PaymentId,
    _quote: &Quote,
) -> bool {
    false
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
