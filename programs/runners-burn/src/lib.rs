//! Permissionless collection and destruction of Token-2022 withheld fees.
//!
//! There is deliberately no transfer, arbitrary CPI, authority update, or admin
//! instruction. The mint opts in by assigning its withdraw-withheld authority to
//! this program's PDA. Deployment upgrade authority remains a separate trust
//! decision; see the review document before making a production deployment.

use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult, instruction::AccountMeta, msg,
    program::invoke_signed, program_error::ProgramError, program_option::COption, pubkey,
    pubkey::Pubkey,
};
use spl_token_2022::{
    extension::{
        transfer_fee::{instruction as fee_instruction, TransferFeeAmount, TransferFeeConfig},
        BaseStateWithExtensions, ExtensionType, StateWithExtensions,
    },
    state::{Account, AccountState, Mint},
};

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

pub const AUTHORITY_SEED: &[u8] = b"burn-authority";
pub const MAX_SOURCES: usize = 32;
pub const BURN_INSTRUCTION: u8 = 0;
const ASSOCIATED_TOKEN_PROGRAM: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum BurnError {
    InvalidInstruction = 0,
    InvalidTokenProgram,
    InvalidAuthority,
    InvalidMint,
    InvalidVault,
    InvalidSource,
    DuplicateAccount,
    TooManySources,
    UnsupportedExtension,
    BalanceMismatch,
}
impl From<BurnError> for ProgramError {
    fn from(value: BurnError) -> Self {
        Self::Custom(value as u32)
    }
}

pub fn authority(program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[AUTHORITY_SEED], program_id)
}

pub fn vault(authority: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            authority.as_ref(),
            spl_token_2022::id().as_ref(),
            mint.as_ref(),
        ],
        &ASSOCIATED_TOKEN_PROGRAM,
    )
    .0
}

/// Wire format: data `[0]`; accounts mint(w), authority, canonical ATA(w),
/// Token-2022 executable, then 0..=32 distinct source accounts(w).
/// The caller creates the canonical ATA beforehand (idempotent ATA instruction).
/// No holder signature or privileged operator is required.
pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if data != [BURN_INSTRUCTION] {
        return Err(BurnError::InvalidInstruction.into());
    }
    if accounts.len() < 4 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    if accounts.len() > 4 + MAX_SOURCES {
        return Err(BurnError::TooManySources.into());
    }
    let mint = &accounts[0];
    let signer = &accounts[1];
    let sink = &accounts[2];
    let token_program = &accounts[3];
    let sources = &accounts[4..];
    if token_program.key != &spl_token_2022::id() || !token_program.executable {
        return Err(BurnError::InvalidTokenProgram.into());
    }
    let (expected_authority, bump) = authority(program_id);
    if signer.key != &expected_authority {
        return Err(BurnError::InvalidAuthority.into());
    }
    if mint.owner != token_program.key || !mint.is_writable {
        return Err(BurnError::InvalidMint.into());
    }
    let (decimals, supply_before, mint_withheld) = {
        let bytes = mint.try_borrow_data()?;
        let state = StateWithExtensions::<Mint>::unpack(&bytes)?;
        // A permanent delegate or transfer hook would invalidate the burn-only
        // vault assumptions. The launchpad's metadata and fee extensions suffice.
        for extension in state.get_extension_types()? {
            if !matches!(
                extension,
                ExtensionType::TransferFeeConfig
                    | ExtensionType::MetadataPointer
                    | ExtensionType::TokenMetadata
            ) {
                return Err(BurnError::UnsupportedExtension.into());
            }
        }
        let fee = state.get_extension::<TransferFeeConfig>()?;
        if Option::<Pubkey>::from(fee.withdraw_withheld_authority) != Some(expected_authority) {
            return Err(BurnError::InvalidAuthority.into());
        }
        (
            state.base.decimals,
            state.base.supply,
            u64::from(fee.withheld_amount),
        )
    };
    if sink.key != &vault(&expected_authority, mint.key)
        || sink.owner != token_program.key
        || !sink.is_writable
    {
        return Err(BurnError::InvalidVault.into());
    }
    let (vault_balance, vault_withheld) = {
        let bytes = sink.try_borrow_data()?;
        let state = StateWithExtensions::<Account>::unpack(&bytes)?;
        if state.base.mint != *mint.key
            || state.base.owner != expected_authority
            || state.base.delegate != COption::None
            || state.base.close_authority != COption::None
            || state.base.state != AccountState::Initialized
        {
            return Err(BurnError::InvalidVault.into());
        }
        (
            state.base.amount,
            u64::from(state.get_extension::<TransferFeeAmount>()?.withheld_amount),
        )
    };
    let mut total = vault_balance
        .checked_add(vault_withheld)
        .and_then(|n| n.checked_add(mint_withheld))
        .ok_or(ProgramError::ArithmeticOverflow)?;
    for (index, source) in sources.iter().enumerate() {
        if accounts[..4].iter().any(|a| a.key == source.key)
            || sources[..index].iter().any(|a| a.key == source.key)
        {
            return Err(BurnError::DuplicateAccount.into());
        }
        if source.owner != token_program.key || !source.is_writable {
            return Err(BurnError::InvalidSource.into());
        }
        let bytes = source.try_borrow_data()?;
        let state = StateWithExtensions::<Account>::unpack(&bytes)?;
        if state.base.mint != *mint.key {
            return Err(BurnError::InvalidSource.into());
        }
        total = total
            .checked_add(u64::from(
                state.get_extension::<TransferFeeAmount>()?.withheld_amount,
            ))
            .ok_or(ProgramError::ArithmeticOverflow)?;
    }
    let bump_seed = [bump];
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &bump_seed];
    // Donations to the vault may themselves have withheld fees. Harvest those
    // first, then withdraw the mint accumulator; neither is an ordinary transfer.
    if vault_withheld != 0 {
        invoke_signed(
            &fee_instruction::harvest_withheld_tokens_to_mint(
                token_program.key,
                mint.key,
                &[sink.key],
            )?,
            &[mint.clone(), sink.clone(), token_program.clone()],
            &[seeds],
        )?;
    }
    if mint_withheld != 0 || vault_withheld != 0 {
        invoke_signed(
            &fee_instruction::withdraw_withheld_tokens_from_mint(
                token_program.key,
                mint.key,
                sink.key,
                signer.key,
                &[],
            )?,
            &[
                mint.clone(),
                sink.clone(),
                signer.clone(),
                token_program.clone(),
            ],
            &[seeds],
        )?;
    }
    if !sources.is_empty() {
        let keys: Vec<&Pubkey> = sources.iter().map(|a| a.key).collect();
        let ix = fee_instruction::withdraw_withheld_tokens_from_accounts(
            token_program.key,
            mint.key,
            sink.key,
            signer.key,
            &[],
            &keys,
        )?;
        let mut infos = vec![mint.clone(), sink.clone(), signer.clone()];
        infos.extend_from_slice(sources);
        infos.push(token_program.clone());
        invoke_signed(&ix, &infos, &[seeds])?;
    }
    // Check the actual post-CPI balance, including the case where Token-2022
    // silently skips an invalid withdrawal source. Never report an unburned fee.
    let available = StateWithExtensions::<Account>::unpack(&sink.try_borrow_data()?)?
        .base
        .amount;
    if available != total {
        return Err(BurnError::BalanceMismatch.into());
    }
    if available != 0 {
        invoke_signed(
            &spl_token_2022::instruction::burn_checked(
                token_program.key,
                sink.key,
                mint.key,
                signer.key,
                &[],
                available,
                decimals,
            )?,
            &[
                sink.clone(),
                mint.clone(),
                signer.clone(),
                token_program.clone(),
            ],
            &[seeds],
        )?;
    }
    let remaining = StateWithExtensions::<Account>::unpack(&sink.try_borrow_data()?)?
        .base
        .amount;
    let supply_after = StateWithExtensions::<Mint>::unpack(&mint.try_borrow_data()?)?
        .base
        .supply;
    if remaining != 0 || supply_before.checked_sub(available) != Some(supply_after) {
        return Err(BurnError::BalanceMismatch.into());
    }
    msg!(
        "runners-burn:v1 mint={} amount={} sources={}",
        mint.key,
        available,
        sources.len()
    );
    Ok(())
}

pub fn instruction(
    program_id: Pubkey,
    mint: Pubkey,
    sources: &[Pubkey],
) -> solana_program::instruction::Instruction {
    let signer = authority(&program_id).0;
    let mut accounts = vec![
        AccountMeta::new(mint, false),
        AccountMeta::new_readonly(signer, false),
        AccountMeta::new(vault(&signer, &mint), false),
        AccountMeta::new_readonly(spl_token_2022::id(), false),
    ];
    accounts.extend(sources.iter().map(|key| AccountMeta::new(*key, false)));
    solana_program::instruction::Instruction {
        program_id,
        accounts,
        data: vec![BURN_INSTRUCTION],
    }
}
