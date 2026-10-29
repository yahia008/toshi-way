//! Balances, withdrawals and the naira ramp, end to end against the mock node and wallets.

use std::sync::Arc;

use escrow_core::{
    mock::{MockLightning, MockWallet},
    model::{BankAccount, Escrow, Preimage, Withdrawal, RampDirection, RampState, WithdrawalKind, WithdrawalState},
    payout::{PaymentStatus, PayoutError, PayoutService},
    ramp::{quote, DemoRamp, RampConfig, RampError, RampService},
    service::{EscrowService, NewEscrow},
    state::EscrowState,
    store::MemoryStore,
    timing::TimingConfig,
};

const T0: i64 = 1_000_000;
const HOUR: i64 = 3600;
/// 97,500,000.00 naira per bitcoin, in kobo.
const NGN_RATE: i64 = 9_750_000_000;

struct Fixture {
    escrows: EscrowService<MockLightning, Arc<MemoryStore>>,
    payouts: Arc<PayoutService<Arc<MemoryStore>>>,
    ramp: RampService<Arc<MemoryStore>>,
    node: MockLightning,
    /// The service's shared node, as it pays sellers out.
    shared: MockWallet,
    /// The ramp provider's own Lightning funds.
    ramp_wallet: MockWallet,
}

fn fixture() -> Fixture {
    let node = MockLightning::new();
    let store = Arc::new(MemoryStore::new());
    let shared = MockWallet::new();
    let ramp_wallet = MockWallet::new();
    let payouts = Arc::new(PayoutService::new(Arc::new(shared.clone()), store.clone()));
    let ramp = RampService::new(
        Arc::new(DemoRamp::new(Arc::new(ramp_wallet.clone()))),
        Some(payouts.clone()),
        store.clone(),
        RampConfig::default(),
    );
    Fixture {
        escrows: EscrowService::new(node.clone(), store, TimingConfig::default()),
        payouts,
        ramp,
        node,
        shared,
        ramp_wallet,
    }
}

fn bank() -> BankAccount {
    BankAccount {
        bank_code: "058".into(),
        bank_name: "Guaranty Trust Bank".into(),
        account_number: "0123456789".into(),
        account_name: "ADA OBI".into(),
    }
}

fn offer(amount_sats: u64) -> NewEscrow {
    NewEscrow {
        buyer_id: Some("buyer".into()),
        seller_id: "seller".into(),
        arbiter: None,
        fiat: None,
        reference: None,
        return_url: None,
        amount_sats,
        description: "deliver the parcel".into(),
        deadline_at: T0 + HOUR,
    }
}

async fn issued(f: &Fixture, amount_sats: u64, p: &Preimage) -> Escrow {
    let e = f.escrows.create(offer(amount_sats), T0).await.unwrap();
    f.escrows.accept(&e.id, p.hash(), "buyer", T0).await.unwrap()
}

/// An escrow on the shared node that the buyer paid and released: the seller is owed its sats.
async fn earned(f: &Fixture, amount_sats: u64, n: u8) {
    let p = Preimage::from_bytes([n; 32]);
    let e = issued(f, amount_sats, &p).await;
    f.escrows.apply_event(f.node.pay(&p.hash()), T0 + 1).await.unwrap();
    let e = f.escrows.release(&e.id, &p, "seller", T0 + 2).await.unwrap();
    assert_eq!(e.state, EscrowState::Settled);
}

#[tokio::test]
async fn a_release_on_the_shared_node_becomes_balance_and_can_be_withdrawn() {
    let f = fixture();
    assert_eq!(f.payouts.balance("seller", T0).await.unwrap().available(), 0);
    earned(&f, 20_000, 1).await;
    let b = f.payouts.balance("seller", T0).await.unwrap();
    assert_eq!((b.earned_sats, b.available()), (20_000, 20_000));

    let invoice = f.shared.invoice(15_000, T0 + HOUR);
    let w = f.payouts.withdraw("seller", &format!("  LIGHTNING:{}  ", invoice.to_uppercase()), WithdrawalKind::Lightning, T0 + 10).await.unwrap();
    assert_eq!((w.state, w.amount_sats, w.fee_limit_sats), (WithdrawalState::Pending, 15_000, 150));
    assert_eq!(f.payouts.balance("seller", T0).await.unwrap().available(), 4_850, "amount and fee allowance held");

    // The node reports it paid, with a 4-sat fee: only that is charged.
    f.shared.set_payment(&w.payment_hash, PaymentStatus::Succeeded { fee_sats: 4 });
    assert_eq!(f.payouts.sync(T0 + 20).await.unwrap().finished, [w.id.clone()]);
    let b = f.payouts.balance("seller", T0).await.unwrap();
    assert_eq!((b.held_sats, b.withdrawn_sats, b.available()), (0, 15_004, 4_996));
    assert!(f.payouts.sync(T0 + 30).await.unwrap().finished.is_empty(), "nothing left to do");
}

#[tokio::test]
async fn withdrawals_are_checked_before_anything_is_paid() {
    let f = fixture();
    earned(&f, 1_000, 1).await;

    let too_much = f.shared.invoice(1_000, T0 + HOUR);
    match f.payouts.withdraw("seller", &too_much, WithdrawalKind::Lightning, T0).await {
        Err(PayoutError::Insufficient { available: 1_000, needed: 1_010 }) => {}
        other => panic!("expected insufficient balance, got {other:?}"),
    }
    let expired = f.shared.invoice(100, T0 + 10);
    assert!(matches!(f.payouts.withdraw("seller", &expired, WithdrawalKind::Lightning, T0).await, Err(PayoutError::Invalid(_))));
    assert!(matches!(f.payouts.withdraw("seller", "lnbcrt1nonsense", WithdrawalKind::Lightning, T0).await, Err(PayoutError::Invalid(_))));
    assert!(matches!(f.payouts.withdraw("seller", "hello", WithdrawalKind::Lightning, T0).await, Err(PayoutError::Invalid(_))));
    // A buyer is owed nothing: paying for something earns no balance.
    let mine = f.shared.invoice(100, T0 + HOUR);
    assert!(matches!(f.payouts.withdraw("buyer", &mine, WithdrawalKind::Lightning, T0).await, Err(PayoutError::Insufficient { .. })));
    assert!(f.shared.paid().is_empty(), "no payment was attempted");

    // The same invoice cannot be paid twice.
    f.payouts.withdraw("seller", &mine, WithdrawalKind::Lightning, T0).await.unwrap();
    assert!(matches!(f.payouts.withdraw("seller", &mine, WithdrawalKind::Lightning, T0).await, Err(PayoutError::Duplicate)));
}

#[tokio::test]
async fn a_failed_payment_gives_the_sats_back() {
    let f = fixture();
    earned(&f, 5_000, 1).await;
    f.shared.next_payment(PaymentStatus::Failed("no route".into()));
    let w = f.payouts.withdraw("seller", &f.shared.invoice(4_000, T0 + HOUR), WithdrawalKind::Lightning, T0).await.unwrap();
    assert_eq!((w.state, w.failure.as_deref()), (WithdrawalState::Failed, Some("no route")));
    assert_eq!(f.payouts.balance("seller", T0).await.unwrap().available(), 5_000);
}

#[tokio::test]
async fn a_payment_lost_in_transit_stays_pending_until_the_node_says_how_it_went() {
    let f = fixture();
    earned(&f, 9_000, 1).await;

    // The node took it after all: once it says so, the withdrawal succeeded.
    let invoice = f.shared.invoice(4_000, T0 + HOUR);
    f.shared.lose_next_payment();
    let w = f.payouts.withdraw("seller", &invoice, WithdrawalKind::Lightning, T0).await.unwrap();
    assert_eq!(w.state, WithdrawalState::Pending, "unknown outcome: the sats stay held");
    f.shared.set_offline(true);
    assert_eq!(f.payouts.sync(T0 + 5).await.unwrap().errors.len(), 1, "an unreachable node is reported, not guessed at");
    f.shared.set_offline(false);
    f.shared.set_payment(&w.payment_hash, PaymentStatus::Succeeded { fee_sats: 1 });
    f.payouts.sync(T0 + 10).await.unwrap();
    assert_eq!(f.payouts.get(&w.id).await.unwrap().unwrap().state, WithdrawalState::Succeeded);

    // The node never heard of it: after a grace period it counts as failed and the sats come back.
    f.shared.lose_next_payment();
    let w = f.payouts.withdraw("seller", &f.shared.invoice(4_000, T0 + HOUR), WithdrawalKind::Lightning, T0 + 20).await.unwrap();
    f.payouts.sync(T0 + 30).await.unwrap();
    assert_eq!(f.payouts.get(&w.id).await.unwrap().unwrap().state, WithdrawalState::Pending, "still within the grace period");
    f.payouts.sync(T0 + 80).await.unwrap();
    let w = f.payouts.get(&w.id).await.unwrap().unwrap();
    assert_eq!(w.state, WithdrawalState::Failed);
    assert_eq!(f.payouts.balance("seller", T0).await.unwrap().available(), 9_000 - 4_001);
}

#[tokio::test]
async fn quotes_add_the_fee_for_buyers_and_take_it_from_sellers() {
    // 5,000 sats at 97,500,000 naira per bitcoin is 4,875.00 naira; 1.5% is 73.125, rounded up.
    let on = quote(RampDirection::On, 5_000, "NGN", NGN_RATE, 150).unwrap();
    assert_eq!((on.fiat_minor, on.fee_minor), (494_813, 7_313));
    let off = quote(RampDirection::Off, 5_000, "NGN", NGN_RATE, 150).unwrap();
    assert_eq!((off.fiat_minor, off.fee_minor), (480_187, 7_313));
    assert!(quote(RampDirection::Off, 1, "NGN", NGN_RATE, 10_000).is_err(), "nothing left after the fee");
}

#[tokio::test]
async fn a_buyer_pays_in_naira_and_the_seller_is_paid_in_sats() {
    let f = fixture();
    let p = Preimage::from_bytes([7; 32]);
    let e = issued(&f, 5_000, &p).await;

    let o = f.ramp.start_onramp("buyer", &e, bank(), NGN_RATE, T0 + 5).await.unwrap();
    assert_eq!((o.state, o.fiat_minor, o.amount_sats), (RampState::AwaitingDeposit, 494_813, 5_000));
    let d = o.deposit.clone().unwrap();
    assert!(d.reference.starts_with("TW-") && d.account_number.len() == 10);
    assert_eq!(d.expires_at, e.invoice_expires_at.unwrap() - 60, "closes before the invoice does");
    // Asking again returns the same order rather than a second one.
    assert_eq!(f.ramp.start_onramp("buyer", &e, bank(), NGN_RATE, T0 + 6).await.unwrap().id, o.id);
    // Nothing happens until the money arrives.
    assert!(f.ramp.tick(T0 + 10).await.unwrap().advanced.is_empty());

    f.ramp.confirm_deposit(&o.id, &bank(), T0 + 20).await.unwrap();
    f.ramp.tick(T0 + 21).await.unwrap();
    assert_eq!(f.ramp.get(&o.id).await.unwrap().state, RampState::Paying);
    assert_eq!(f.ramp_wallet.paid(), [p.hash()], "the provider paid the escrow's invoice");

    // The node reports the payment held: the escrow is locked, and so is the order.
    f.escrows.apply_event(f.node.pay(&p.hash()), T0 + 22).await.unwrap();
    f.ramp.tick(T0 + 23).await.unwrap();
    assert_eq!(f.ramp.get(&o.id).await.unwrap().state, RampState::Funded);

    // Released: the provider's payment completes and the seller has the sats.
    f.escrows.release(&e.id, &p, "seller", T0 + 30).await.unwrap();
    f.ramp_wallet.set_payment(&p.hash(), PaymentStatus::Succeeded { fee_sats: 1 });
    f.ramp.tick(T0 + 31).await.unwrap();
    assert_eq!(f.ramp.get(&o.id).await.unwrap().state, RampState::Complete);
    // Paid in naira, so the sats wait out the time a bank transfer can still be reversed.
    let b = f.payouts.balance("seller", T0 + 31).await.unwrap();
    assert_eq!((b.earned_sats, b.maturing_sats, b.available()), (5_000, 5_000, 0));
    assert!(matches!(
        f.payouts.withdraw("seller", &f.shared.invoice(1_000, T0 + 3_600), WithdrawalKind::Lightning, T0 + 31).await,
        Err(PayoutError::Insufficient { available: 0, .. })
    ));
    let a_day_later = T0 + 30 + 24 * 3_600;
    assert_eq!(f.payouts.balance("seller", a_day_later).await.unwrap().available(), 5_000);
}

#[tokio::test]
async fn withdrawals_are_capped_per_day() {
    let f = fixture();
    earned(&f, 900_000, 1).await;
    earned(&f, 900_000, 2).await;
    async fn pay(f: &Fixture, amount: u64, at: i64) -> Result<Withdrawal, PayoutError> {
        f.payouts.withdraw("seller", &f.shared.invoice(amount, at + 3_600), WithdrawalKind::Lightning, at).await
    }
    pay(&f, 600_000, T0).await.unwrap();
    // 600,000 + 500,000 would pass the 1,000,000-sat cap for the last 24 hours.
    assert!(matches!(pay(&f, 500_000, T0 + 1).await, Err(PayoutError::OverLimit { left: 400_000, hours: 24 })));
    pay(&f, 400_000, T0 + 2).await.unwrap();
    // A day later the first one no longer counts.
    pay(&f, 500_000, T0 + 24 * 3_600 + 1).await.unwrap();
}

#[tokio::test]
async fn a_refunded_escrow_sends_the_naira_back_to_the_buyers_bank() {
    let f = fixture();
    let p = Preimage::from_bytes([8; 32]);
    let e = issued(&f, 5_000, &p).await;
    let o = f.ramp.start_onramp("buyer", &e, bank(), NGN_RATE, T0).await.unwrap();
    f.ramp.confirm_deposit(&o.id, &bank(), T0 + 1).await.unwrap();
    f.ramp.tick(T0 + 2).await.unwrap();
    f.escrows.apply_event(f.node.pay(&p.hash()), T0 + 3).await.unwrap();
    f.ramp.tick(T0 + 4).await.unwrap();

    // The deadline passes; the hold is cancelled and the provider's payment fails back to it.
    f.escrows.tick(T0 + HOUR).await.unwrap();
    assert_eq!(f.escrows.get(&e.id).await.unwrap().state, EscrowState::Refunded);
    f.ramp_wallet.set_payment(&p.hash(), PaymentStatus::Failed("incorrect payment details".into()));
    f.ramp.tick(T0 + HOUR + 1).await.unwrap();
    assert_eq!(f.ramp.get(&o.id).await.unwrap().state, RampState::Refunding);
    f.ramp.tick(T0 + HOUR + 2).await.unwrap();
    let o = f.ramp.get(&o.id).await.unwrap();
    assert_eq!(o.state, RampState::Refunded);
    assert!(o.fiat_reference.unwrap().starts_with("DEMO-REFUND-"));
}

#[tokio::test]
async fn a_refund_goes_to_the_account_the_money_came_from_not_the_one_typed() {
    let f = fixture();
    let p = Preimage::from_bytes([18; 32]);
    let e = issued(&f, 5_000, &p).await;
    let o = f.ramp.start_onramp("buyer", &e, bank(), NGN_RATE, T0).await.unwrap();
    let payer = BankAccount {
        bank_code: "057".into(),
        bank_name: "Zenith Bank".into(),
        account_number: "2099999999".into(),
        account_name: "SOMEONE ELSE".into(),
    };
    f.ramp.confirm_deposit(&o.id, &payer, T0 + 1).await.unwrap();
    // The escrow can no longer be paid (cancelled), so the money goes back: to the payer.
    f.escrows.cancel(&e.id, "buyer", "changed my mind", T0 + 2).await.unwrap();
    f.ramp.tick(T0 + 3).await.unwrap();
    f.ramp.tick(T0 + 4).await.unwrap();
    let o = f.ramp.get(&o.id).await.unwrap();
    assert_eq!(o.state, RampState::Refunded);
    assert_eq!(o.bank, payer);
}

#[tokio::test]
async fn no_transfer_in_time_and_the_order_lapses() {
    let f = fixture();
    let p = Preimage::from_bytes([9; 32]);
    let e = issued(&f, 5_000, &p).await;
    let o = f.ramp.start_onramp("buyer", &e, bank(), NGN_RATE, T0).await.unwrap();
    let closes = o.deposit.as_ref().unwrap().expires_at;
    assert!(f.ramp.confirm_deposit(&o.id, &bank(), closes).await.is_err(), "too late to say it arrived");
    f.ramp.tick(closes).await.unwrap();
    assert_eq!(f.ramp.get(&o.id).await.unwrap().state, RampState::Expired);
    assert!(f.ramp_wallet.paid().is_empty());
}

#[tokio::test]
async fn naira_that_arrives_after_the_buyer_paid_another_way_goes_back() {
    let f = fixture();
    let p = Preimage::from_bytes([10; 32]);
    let e = issued(&f, 5_000, &p).await;
    let o = f.ramp.start_onramp("buyer", &e, bank(), NGN_RATE, T0).await.unwrap();
    // The buyer pays from a wallet after all, then the transfer lands too.
    f.escrows.apply_event(f.node.pay(&p.hash()), T0 + 1).await.unwrap();
    f.ramp.confirm_deposit(&o.id, &bank(), T0 + 2).await.unwrap();
    f.ramp.tick(T0 + 3).await.unwrap();
    assert_eq!(f.ramp.get(&o.id).await.unwrap().state, RampState::Refunding);
    assert!(f.ramp_wallet.paid().is_empty(), "the provider did not pay a second time");
}

#[tokio::test]
async fn only_the_buyer_of_an_issued_invoice_can_pay_in_naira() {
    let f = fixture();
    let p = Preimage::from_bytes([11; 32]);
    let e = issued(&f, 5_000, &p).await;
    assert!(matches!(f.ramp.start_onramp("seller", &e, bank(), NGN_RATE, T0).await, Err(RampError::Invalid(_))));
    let mut wrong = bank();
    wrong.account_number = "123".into();
    assert!(matches!(f.ramp.start_onramp("buyer", &e, wrong, NGN_RATE, T0).await, Err(RampError::Invalid(_))));
    // Too close to the invoice's expiry for a transfer to make it.
    let late = e.invoice_expires_at.unwrap() - 100;
    assert!(matches!(f.ramp.start_onramp("buyer", &e, bank(), NGN_RATE, late).await, Err(RampError::Invalid(_))));
    let fresh = f.escrows.create(offer(5_000), T0).await.unwrap();
    assert!(matches!(f.ramp.start_onramp("buyer", &fresh, bank(), NGN_RATE, T0).await, Err(RampError::Invalid(_))));
}

#[tokio::test]
async fn a_seller_withdraws_to_their_bank_in_naira() {
    let f = fixture();
    earned(&f, 10_000, 1).await;
    let o = f.ramp.start_offramp("seller", 5_000, bank(), NGN_RATE, T0 + 10).await.unwrap();
    assert_eq!((o.state, o.fiat_minor), (RampState::SendingSats, 480_187));
    assert_eq!(f.payouts.balance("seller", T0).await.unwrap().available(), 4_950, "5,000 plus a 50-sat fee allowance held");

    // Nothing is paid into the bank until the sats have reached the provider.
    f.ramp.tick(T0 + 11).await.unwrap();
    assert_eq!(f.ramp.get(&o.id).await.unwrap().state, RampState::SendingSats);
    let w = f.payouts.get(o.withdrawal_id.as_deref().unwrap()).await.unwrap().unwrap();
    assert_eq!(w.kind, WithdrawalKind::Bank);
    f.shared.set_payment(&w.payment_hash, PaymentStatus::Succeeded { fee_sats: 2 });
    f.payouts.sync(T0 + 12).await.unwrap();
    f.ramp.tick(T0 + 13).await.unwrap();
    assert_eq!(f.ramp.get(&o.id).await.unwrap().state, RampState::PayingOut);
    f.ramp.tick(T0 + 14).await.unwrap();
    let o = f.ramp.get(&o.id).await.unwrap();
    assert_eq!(o.state, RampState::PaidOut);
    assert!(o.fiat_reference.unwrap().starts_with("DEMO-PAYOUT-"));
    assert_eq!(f.payouts.balance("seller", T0).await.unwrap().available(), 4_998);
}

#[tokio::test]
async fn a_bank_withdrawal_whose_sats_cannot_be_sent_fails_and_keeps_the_balance() {
    let f = fixture();
    earned(&f, 10_000, 1).await;
    f.shared.next_payment(PaymentStatus::Failed("no route".into()));
    let o = f.ramp.start_offramp("seller", 5_000, bank(), NGN_RATE, T0).await.unwrap();
    f.ramp.tick(T0 + 1).await.unwrap();
    let o = f.ramp.get(&o.id).await.unwrap();
    assert_eq!(o.state, RampState::Failed);
    assert!(o.note.unwrap().contains("back in your balance"));
    assert_eq!(f.payouts.balance("seller", T0).await.unwrap().available(), 10_000);
    // More than the balance is refused before any order exists.
    assert!(matches!(
        f.ramp.start_offramp("seller", 20_000, bank(), NGN_RATE, T0).await,
        Err(RampError::Payout(PayoutError::Insufficient { .. }))
    ));
    assert_eq!(f.ramp.list("seller").await.unwrap().len(), 1);
}
