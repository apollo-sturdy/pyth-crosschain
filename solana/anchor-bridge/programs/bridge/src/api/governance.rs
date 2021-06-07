use solitaire::*;

use solana_program::{self,};

use crate::types::{
    self,
    GovernancePayloadSetMessageFee,
    GovernancePayloadTransferFees,
};

use solana_program::pubkey::Pubkey;
use solitaire::{
    processors::seeded::Seeded,
    CreationLamports::Exempt,
};

use crate::{
    types::{
        BridgeData,
        GovernancePayloadGuardianSetChange,
        GovernancePayloadUpgrade,
    },
    vaa::ClaimableVAA,
    Error::{
        InvalidFeeRecipient,
        InvalidGuardianSetUpgrade,
    },
};
use solana_program::program::invoke_signed;

#[derive(FromAccounts)]
pub struct UpgradeContract<'b> {
    /// Payer for account creation (vaa-claim)
    pub payer: Signer<Info<'b>>,

    /// Upgrade VAA
    pub vaa: ClaimableVAA<'b, GovernancePayloadUpgrade>,

    /// PDA authority for the loader
    pub upgrade_authority: Derive<Info<'b>, "upgrade">,

    /// Spill address for the upgrade excess lamports
    pub spill: Info<'b>,
}

impl<'b> InstructionContext<'b> for UpgradeContract<'b> {
}

#[derive(BorshDeserialize, BorshSerialize, Default)]
pub struct UpgradeContractData {}

pub fn upgrade_contract(
    ctx: &ExecutionContext,
    accs: &mut UpgradeContract,
    _data: UpgradeContractData,
) -> Result<()> {
    accs.vaa.claim(ctx, accs.payer.key)?;

    let upgrade_ix = solana_program::bpf_loader_upgradeable::upgrade(
        ctx.program_id,
        &accs.vaa.message.new_contract,
        accs.upgrade_authority.key,
        accs.spill.key,
    );
    let _seeds = accs.upgrade_authority.seeds(None);
    invoke_signed(&upgrade_ix, ctx.accounts, &[])?;

    Ok(())
}

type GuardianSet<'b> = Data<'b, types::GuardianSetData, { AccountState::Initialized }>;
type GuardianSetNew<'b> = Data<'b, types::GuardianSetData, { AccountState::Uninitialized }>;

impl<'b> Seeded<&UpgradeGuardianSet<'b>> for GuardianSet<'b> {
    fn seeds(&self, accs: &UpgradeGuardianSet<'b>) -> Vec<Vec<u8>> {
        vec![(accs.vaa.new_guardian_set_index - 1).to_be_bytes().to_vec()]
    }
}

impl<'b> Seeded<&UpgradeGuardianSet<'b>> for GuardianSetNew<'b> {
    fn seeds(&self, accs: &UpgradeGuardianSet<'b>) -> Vec<Vec<u8>> {
        vec![accs.vaa.new_guardian_set_index.to_be_bytes().to_vec()]
    }
}

pub type Bridge<'a> = Derive<Data<'a, BridgeData, { AccountState::Initialized }>, "Bridge">;

#[derive(FromAccounts)]
pub struct UpgradeGuardianSet<'b> {
    /// Payer for account creation (vaa-claim)
    pub payer: Signer<Info<'b>>,

    /// Bridge config
    pub bridge: Bridge<'b>,

    /// GuardianSet change VAA
    pub vaa: ClaimableVAA<'b, GovernancePayloadGuardianSetChange>,

    /// Old guardian set
    pub guardian_set_old: GuardianSet<'b>,
    /// New guardian set
    pub guardian_set_new: GuardianSetNew<'b>,
}

impl<'b> InstructionContext<'b> for UpgradeGuardianSet<'b> {
    fn verify(&self, _program_id: &Pubkey) -> Result<()> {
        if self.guardian_set_old.index != self.vaa.new_guardian_set_index - 1 {
            return Err(InvalidGuardianSetUpgrade.into());
        }

        if self.bridge.guardian_set_index != self.vaa.new_guardian_set_index - 1 {
            return Err(InvalidGuardianSetUpgrade.into());
        }

        Ok(())
    }
}

#[derive(BorshDeserialize, BorshSerialize, Default)]
pub struct UpgradeGuardianSetData {}

pub fn upgrade_guardian_set(
    ctx: &ExecutionContext,
    accs: &mut UpgradeGuardianSet,
    _data: UpgradeGuardianSetData,
) -> Result<()> {
    accs.vaa.claim(ctx, accs.payer.key)?;

    // Set expiration time for the old set
    accs.guardian_set_old.expiration_time =
        accs.vaa.meta().vaa_time + accs.bridge.config.guardian_set_expiration_time;

    // Initialize new guardian Set
    accs.guardian_set_new.index = accs.vaa.new_guardian_set_index;
    accs.guardian_set_new.creation_time = accs.vaa.meta().vaa_time;
    accs.guardian_set_new.keys = accs.vaa.new_guardian_set.clone();

    // Create new guardian set
    // This is done after populating it to properly allocate space according to key vec length.
    accs.guardian_set_new
        .create(accs, ctx, accs.payer.key, Exempt)?;

    // Set guardian set index
    accs.bridge.guardian_set_index = accs.vaa.new_guardian_set_index;

    Ok(())
}

#[derive(FromAccounts)]
pub struct SetFees<'b> {
    /// Payer for account creation (vaa-claim)
    pub payer: Signer<Info<'b>>,

    /// Bridge config
    pub bridge: Bridge<'b>,

    /// Governance VAA
    pub vaa: ClaimableVAA<'b, GovernancePayloadSetMessageFee>,
}

impl<'b> InstructionContext<'b> for SetFees<'b> {
}

#[derive(BorshDeserialize, BorshSerialize, Default)]
pub struct SetFeesData {}

pub fn set_fees(ctx: &ExecutionContext, accs: &mut SetFees, _data: SetFeesData) -> Result<()> {
    accs.vaa.claim(ctx, accs.payer.key)?;

    // Set expiration time for the old set
    accs.bridge.config.fee = accs.vaa.fee;

    Ok(())
}

#[derive(FromAccounts)]
pub struct TransferFees<'b> {
    /// Payer for account creation (vaa-claim)
    pub payer: Signer<Info<'b>>,

    /// Bridge config
    pub bridge: Bridge<'b>,

    /// Governance VAA
    pub vaa: ClaimableVAA<'b, GovernancePayloadTransferFees>,

    /// Account collecting tx fees
    pub fee_collector: Derive<Info<'b>, "fee_collector">,

    /// Fee recipient
    pub recipient: Info<'b>,
}

impl<'b> InstructionContext<'b> for TransferFees<'b> {
    fn verify(&self, _program_id: &Pubkey) -> Result<()> {
        if self.vaa.to != self.recipient.key.to_bytes() {
            return Err(InvalidFeeRecipient.into());
        }

        Ok(())
    }
}

#[derive(BorshDeserialize, BorshSerialize, Default)]
pub struct TransferFeesData {}

pub fn transfer_fees(
    ctx: &ExecutionContext,
    accs: &mut TransferFees,
    _data: TransferFeesData,
) -> Result<()> {
    accs.vaa.claim(ctx, accs.payer.key)?;

    // Transfer fees
    let transfer_ix = solana_program::system_instruction::transfer(
        accs.fee_collector.key,
        accs.recipient.key,
        accs.vaa.amount.as_u64(),
    );
    let seeds = accs.fee_collector.seeds(None);
    let s: Vec<&[u8]> = seeds.iter().map(|item| item.as_slice()).collect();
    let seed_slice = s.as_slice();
    invoke_signed(&transfer_ix, ctx.accounts, &[seed_slice])?;

    Ok(())
}
