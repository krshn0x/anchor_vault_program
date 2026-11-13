use {
    anchor_lang::{
        prelude::{AccountDeserialize, Pubkey},
        solana_program::{
            instruction::{error::InstructionError, Instruction},
            system_program,
        },
        InstructionData, ToAccountMetas,
    },
    litesvm::{types::TransactionResult, LiteSVM},
    q3_26_vault::{error::ErrorCode, VaultState, STATE, VAULT_SEED},
    solana_keypair::Keypair,
    solana_message::{Message, VersionedMessage},
    solana_signer::Signer,
    solana_transaction::{versioned::VersionedTransaction, TransactionError},
};

const DEPOSIT_LAMPORTS: u64 = 500_000_000;
const WITHDRAW_LAMPORTS: u64 = 100_000_000;

fn send(svm: &mut LiteSVM, payer: &Keypair, ix: Instruction) -> TransactionResult {
    // Repeated instructions must get distinct signatures, including failed attempts.
    svm.expire_blockhash();
    let msg = Message::new_with_blockhash(&[ix], Some(&payer.pubkey()), &svm.latest_blockhash());
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[payer]).unwrap();
    svm.send_transaction(tx)
}

fn assert_custom_error(result: TransactionResult, code: u32) {
    let err = result.expect_err("transaction should fail");
    assert_eq!(
        err.err,
        TransactionError::InstructionError(0, InstructionError::Custom(code)),
        "unexpected failure: {:#?}",
        err.meta.logs
    );
}

struct VaultFixture {
    svm: LiteSVM,
    user: Keypair,
    vault_state: Pubkey,
    vault: Pubkey,
}

impl VaultFixture {
    fn new() -> Self {
        let user = Keypair::new();
        let program_id = q3_26_vault::id();
        let (vault_state, _) =
            Pubkey::find_program_address(&[STATE, user.pubkey().as_ref()], &program_id);
        let (vault, _) =
            Pubkey::find_program_address(&[VAULT_SEED, user.pubkey().as_ref()], &program_id);
        let mut svm = LiteSVM::new();
        let bytes = std::fs::read(concat!(
            env!("CARGO_TARGET_TMPDIR"),
            "/../deploy/q3_26_vault.so"
        ))
        .expect("program binary is missing; run `anchor build` before running tests");
        svm.add_program(program_id, &bytes).unwrap();
        svm.airdrop(&user.pubkey(), 2_000_000_000).unwrap();
        Self {
            svm,
            user,
            vault_state,
            vault,
        }
    }

    fn initialized() -> Self {
        let mut fixture = Self::new();
        fixture.execute(fixture.initialize_ix()).unwrap();
        fixture
    }

    fn execute(&mut self, ix: Instruction) -> TransactionResult {
        send(&mut self.svm, &self.user, ix)
    }

    fn instruction(
        &self,
        data: impl InstructionData,
        accounts: impl ToAccountMetas,
    ) -> Instruction {
        Instruction::new_with_bytes(
            q3_26_vault::id(),
            &data.data(),
            accounts.to_account_metas(None),
        )
    }

    fn initialize_ix(&self) -> Instruction {
        self.instruction(
            q3_26_vault::instruction::Initialize {},
            q3_26_vault::accounts::Initialize {
                user: self.user.pubkey(),
                vault_state: self.vault_state,
                vault: self.vault,
                system_program: system_program::ID,
            },
        )
    }

    fn deposit_ix(&self, amount: u64) -> Instruction {
        self.instruction(
            q3_26_vault::instruction::Deposit { amount },
            q3_26_vault::accounts::Deposit {
                user: self.user.pubkey(),
                vault_state: self.vault_state,
                vault: self.vault,
                system_program: system_program::ID,
            },
        )
    }

    fn withdraw_ix(&self, amount: u64) -> Instruction {
        self.instruction(
            q3_26_vault::instruction::Withdraw { amount },
            q3_26_vault::accounts::Withdraw {
                user: self.user.pubkey(),
                vault_state: self.vault_state,
                vault: self.vault,
                system_program: system_program::ID,
            },
        )
    }

    fn close_ix(&self) -> Instruction {
        self.instruction(
            q3_26_vault::instruction::Close {},
            q3_26_vault::accounts::Close {
                user: self.user.pubkey(),
                vault_state: self.vault_state,
                vault: self.vault,
                system_program: system_program::ID,
            },
        )
    }

    fn vault_balance(&self) -> u64 {
        self.svm.get_balance(&self.vault).unwrap_or(0)
    }
}

#[test]
fn complete_vault_lifecycle() {
    let mut fixture = VaultFixture::initialized();
    let rent = fixture.svm.minimum_balance_for_rent_exemption(0);
    assert_eq!(fixture.vault_balance(), rent);

    let state_account = fixture.svm.get_account(&fixture.vault_state).unwrap();
    assert_eq!(state_account.owner, q3_26_vault::id());
    let state = VaultState::try_deserialize(&mut state_account.data.as_slice()).unwrap();
    let user = fixture.user.pubkey();
    assert_eq!(
        Pubkey::create_program_address(
            &[STATE, user.as_ref(), &[state.state_bump]],
            &q3_26_vault::id(),
        )
        .unwrap(),
        fixture.vault_state
    );
    assert_eq!(
        Pubkey::create_program_address(
            &[VAULT_SEED, user.as_ref(), &[state.vault_bump]],
            &q3_26_vault::id(),
        )
        .unwrap(),
        fixture.vault
    );

    fixture
        .execute(fixture.deposit_ix(DEPOSIT_LAMPORTS))
        .unwrap();
    assert_eq!(fixture.vault_balance(), rent + DEPOSIT_LAMPORTS);

    let user_before = fixture.svm.get_balance(&user).unwrap();
    let withdrawal = fixture
        .execute(fixture.withdraw_ix(WITHDRAW_LAMPORTS))
        .unwrap();
    assert_eq!(
        fixture.svm.get_balance(&user).unwrap(),
        user_before + WITHDRAW_LAMPORTS - withdrawal.fee
    );
    assert_eq!(
        fixture.vault_balance(),
        rent + DEPOSIT_LAMPORTS - WITHDRAW_LAMPORTS
    );

    let user_before = fixture.svm.get_balance(&user).unwrap();
    let vault_before = fixture.vault_balance();
    let close = fixture.execute(fixture.close_ix()).unwrap();
    assert_eq!(fixture.vault_balance(), 0);
    assert!(fixture.svm.get_account(&fixture.vault_state).is_none());
    assert_eq!(
        fixture.svm.get_balance(&user).unwrap(),
        user_before + vault_before + state_account.lamports - close.fee,
        "closing must return the entire vault balance and state rent"
    );
}

#[test]
fn zero_deposits_and_withdrawals_are_rejected() {
    let mut fixture = VaultFixture::initialized();
    let balance = fixture.vault_balance();
    assert_custom_error(
        fixture.execute(fixture.deposit_ix(0)),
        ErrorCode::InvalidAmount.into(),
    );
    assert_custom_error(
        fixture.execute(fixture.withdraw_ix(0)),
        ErrorCode::InvalidAmount.into(),
    );
    assert_eq!(fixture.vault_balance(), balance);
}

#[test]
fn withdrawals_preserve_rent_and_reject_overdrafts() {
    let mut fixture = VaultFixture::initialized();
    let rent = fixture.vault_balance();
    assert_custom_error(
        fixture.execute(fixture.withdraw_ix(1)),
        ErrorCode::InsufficientFunds.into(),
    );
    fixture
        .execute(fixture.deposit_ix(DEPOSIT_LAMPORTS))
        .unwrap();
    for amount in [DEPOSIT_LAMPORTS + 1, u64::MAX] {
        assert_custom_error(
            fixture.execute(fixture.withdraw_ix(amount)),
            ErrorCode::InsufficientFunds.into(),
        );
        assert_eq!(fixture.vault_balance(), rent + DEPOSIT_LAMPORTS);
    }
    fixture
        .execute(fixture.withdraw_ix(DEPOSIT_LAMPORTS))
        .unwrap();
    assert_eq!(fixture.vault_balance(), rent);
    assert!(fixture.svm.get_account(&fixture.vault_state).is_some());
}

#[test]
fn another_signer_cannot_access_the_vault() {
    let mut fixture = VaultFixture::initialized();
    fixture
        .execute(fixture.deposit_ix(DEPOSIT_LAMPORTS))
        .unwrap();
    let balance = fixture.vault_balance();
    let attacker = Keypair::new();
    fixture
        .svm
        .airdrop(&attacker.pubkey(), 1_000_000_000)
        .unwrap();
    for mut ix in [
        fixture.deposit_ix(1),
        fixture.withdraw_ix(1),
        fixture.close_ix(),
    ] {
        ix.accounts[0].pubkey = attacker.pubkey();
        assert_custom_error(
            send(&mut fixture.svm, &attacker, ix),
            anchor_lang::error::ErrorCode::ConstraintSeeds.into(),
        );
        assert_eq!(fixture.vault_balance(), balance);
        assert!(fixture.svm.get_account(&fixture.vault_state).is_some());
    }
}

#[test]
fn owner_signature_is_required() {
    let mut fixture = VaultFixture::initialized();
    let attacker = Keypair::new();
    fixture
        .svm
        .airdrop(&attacker.pubkey(), 1_000_000_000)
        .unwrap();
    let balance = fixture.vault_balance();
    for mut ix in [fixture.withdraw_ix(1), fixture.close_ix()] {
        ix.accounts[0].is_signer = false;
        assert_custom_error(
            send(&mut fixture.svm, &attacker, ix),
            anchor_lang::error::ErrorCode::AccountNotSigner.into(),
        );
        assert_eq!(fixture.vault_balance(), balance);
    }
}

#[test]
fn substituted_vault_is_rejected() {
    let mut fixture = VaultFixture::initialized();
    let other = Keypair::new();
    fixture.svm.airdrop(&other.pubkey(), 1_000_000_000).unwrap();
    let balance = fixture.vault_balance();
    for mut ix in [
        fixture.deposit_ix(1),
        fixture.withdraw_ix(1),
        fixture.close_ix(),
    ] {
        ix.accounts[2].pubkey = other.pubkey();
        assert_custom_error(
            fixture.execute(ix),
            anchor_lang::error::ErrorCode::ConstraintSeeds.into(),
        );
        assert_eq!(fixture.vault_balance(), balance);
    }
}

#[test]
fn closing_an_empty_vault_and_reopening_work() {
    let mut fixture = VaultFixture::initialized();
    fixture.execute(fixture.close_ix()).unwrap();
    assert_eq!(fixture.vault_balance(), 0);
    assert!(fixture.svm.get_account(&fixture.vault_state).is_none());
    for ix in [
        fixture.deposit_ix(1),
        fixture.withdraw_ix(1),
        fixture.close_ix(),
    ] {
        assert_custom_error(
            fixture.execute(ix),
            anchor_lang::error::ErrorCode::AccountNotInitialized.into(),
        );
    }
    fixture.execute(fixture.initialize_ix()).unwrap();
    assert_eq!(
        fixture.vault_balance(),
        fixture.svm.minimum_balance_for_rent_exemption(0)
    );
    fixture.execute(fixture.deposit_ix(1)).unwrap();
    fixture.execute(fixture.withdraw_ix(1)).unwrap();
    fixture.execute(fixture.close_ix()).unwrap();
}

#[test]
fn duplicate_initialization_is_rejected_without_changing_state() {
    let mut fixture = VaultFixture::initialized();
    fixture
        .execute(fixture.deposit_ix(DEPOSIT_LAMPORTS))
        .unwrap();
    let state_before = fixture.svm.get_account(&fixture.vault_state).unwrap();
    let balance_before = fixture.vault_balance();
    assert!(fixture.execute(fixture.initialize_ix()).is_err());
    assert_eq!(fixture.vault_balance(), balance_before);
    assert_eq!(
        fixture.svm.get_account(&fixture.vault_state).unwrap(),
        state_before
    );
}

#[test]
fn initialization_preserves_prefunded_vault_balance() {
    let mut fixture = VaultFixture::new();
    let prefunded = 2 * fixture.svm.minimum_balance_for_rent_exemption(0);
    fixture.svm.airdrop(&fixture.vault, prefunded).unwrap();
    let user_before = fixture.svm.get_balance(&fixture.user.pubkey()).unwrap();
    let initialize = fixture.execute(fixture.initialize_ix()).unwrap();
    let state = fixture.svm.get_account(&fixture.vault_state).unwrap();
    assert_eq!(fixture.vault_balance(), prefunded);
    assert_eq!(
        fixture.svm.get_balance(&fixture.user.pubkey()).unwrap(),
        user_before - state.lamports - initialize.fee,
        "a prefunded vault must not charge rent twice"
    );
    fixture.execute(fixture.close_ix()).unwrap();
}
