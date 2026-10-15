# runners-burn

The Solana program that burns the $RUNNERS transfer fee.

$RUNNERS is a Token-2022 token with a 1% transfer fee. Token-2022 withholds that fee in the receiving token accounts,
and only the mint's *withdraw-withheld authority* can collect it. That authority is this program's address
`["burn-authority"]` (a PDA), and no key can sign for it, so the fees can only ever be collected by this program,
which burns them.

## What it does

One permissionless instruction. Anyone can call it, and it always does the same thing:

1. withdraws the fees withheld in up to 32 token accounts, and in the mint, into the program's own token account
   (the canonical associated token account of the PDA);
2. burns that account's whole balance.

It never touches anyone's balance, only withheld fees. It cannot send tokens anywhere, and it never changes the
withdraw-withheld authority, so nobody can redirect the fees. Mints with extensions other than the transfer fee and
metadata (for example a permanent delegate or a transfer hook) are refused.

## Build

The deployed program is built in Solana's verifiable-build environment, so anyone can rebuild it byte for byte:

```bash
solana-verify build --base-image solanafoundation/solana-verifiable-build:4.3.0 --library-name runners_burn
```
