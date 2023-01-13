use super::pallet::{self, *};
use crate::origin::{ensure_multisig, INV4Origin};
use crate::util::derive_ips_account;
use core::convert::TryInto;
use frame_support::{
    dispatch::{CallMetadata, GetCallMetadata, GetDispatchInfo},
    pallet_prelude::*,
    traits::WrapperKeepOpaque,
};
use frame_system::{ensure_signed, pallet_prelude::*};
use primitives::{OneOrPercent, Parentage, SubIptInfo};
use sp_io::hashing::blake2_256;
use sp_runtime::traits::{CheckedAdd, CheckedSub};
use sp_std::{boxed::Box, vec, vec::Vec};

pub type OpaqueCall<T> = WrapperKeepOpaque<<T as Config>::Call>;

/// Details of a multisig operation
#[derive(Clone, Encode, Decode, RuntimeDebug, MaxEncodedLen, TypeInfo)]
pub struct MultisigOperation<AccountId, Signers, Call, Metadata> {
    signers: Signers,
    include_original_caller: bool,
    original_caller: AccountId,
    actual_call: Call,
    call_metadata: [u8; 2],
    call_weight: Weight,
    metadata: Option<Metadata>,
}

pub type MultisigOperationOf<T> = MultisigOperation<
    <T as frame_system::Config>::AccountId,
    BoundedVec<
        (
            <T as frame_system::Config>::AccountId,
            // Token account voted with???
            Option<<T as pallet::Config>::IpId>,
        ),
        <T as Config>::MaxCallers,
    >,
    OpaqueCall<T>,
    BoundedVec<u8, <T as pallet::Config>::MaxMetadata>,
>;

pub type SubAssetsWithEndowment<T> = Vec<(
    SubIptInfo<<T as pallet::Config>::IpId, BoundedVec<u8, <T as pallet::Config>::MaxMetadata>>,
    (
        <T as frame_system::Config>::AccountId,
        <T as pallet::Config>::Balance,
    ),
)>;

impl<T: Config> Pallet<T>
where
    Result<
        INV4Origin<T, <T as pallet::Config>::IpId, <T as frame_system::Config>::AccountId>,
        <T as frame_system::Config>::Origin,
    >: From<<T as frame_system::Config>::Origin>,
{
    /// Mint `amount` of specified token to `target` account
    pub(crate) fn inner_ipt_mint(
        origin: OriginFor<T>,
        sub_token: Option<T::IpId>,
        amount: <T as pallet::Config>::Balance,
        target: T::AccountId,
    ) -> DispatchResult {
        let ip_set = ensure_multisig::<T, OriginFor<T>>(origin)?;

        // IP Set must exist for there to be a token
        let ip = IpStorage::<T>::get(ip_set.id).ok_or(Error::<T>::IpDoesntExist)?;

        // Cannot mint IP Tokens on `Parentage::Child` assets or `IpsType::Replica` IP Sets
        match &ip.parentage {
            Parentage::Parent(ips_account) => {
                ensure!(
                    ips_account
                        == &derive_ips_account::<
                            T,
                            <T as pallet::Config>::IpId,
                            <T as frame_system::Config>::AccountId,
                        >(ip_set.id, None),
                    Error::<T>::NoPermission
                )
            }
            Parentage::Child(..) => return Err(Error::<T>::NotParent.into()),
        }

        // If trying to mint more of a sub token, token must already exist
        if let Some(sub) = sub_token {
            ensure!(
                SubAssets::<T>::get(ip_set.id, sub).is_some(),
                Error::<T>::SubAssetNotFound
            );
        }

        // Actually mint tokens
        Pallet::<T>::internal_mint((ip_set.id, sub_token), target.clone(), amount)?;

        Self::deposit_event(Event::Minted {
            token: (ip_set.id, sub_token),
            target,
            amount,
        });

        Ok(())
    }

    /// Burn `amount` of specified token from `target` account
    pub(crate) fn inner_ipt_burn(
        origin: OriginFor<T>,
        sub_token: Option<T::IpId>,
        amount: <T as pallet::Config>::Balance,
        target: T::AccountId,
    ) -> DispatchResult {
        let ip_set = ensure_multisig::<T, OriginFor<T>>(origin)?;

        // IP Set must exist for their to be a token
        let ip = IpStorage::<T>::get(ip_set.id).ok_or(Error::<T>::IpDoesntExist)?;

        // Cannot burn IP Tokens on `Parentage::Child` assets or `IpsType::Replica` IP Sets
        match &ip.parentage {
            Parentage::Parent(ips_account) => {
                ensure!(
                    ips_account
                        == &derive_ips_account::<
                            T,
                            <T as pallet::Config>::IpId,
                            <T as frame_system::Config>::AccountId,
                        >(ip_set.id, None),
                    Error::<T>::NoPermission
                )
            }
            Parentage::Child(..) => return Err(Error::<T>::NotParent.into()),
        }

        // If trying to burn sub tokens, token must already exist
        if let Some(sub) = sub_token {
            ensure!(
                SubAssets::<T>::get(ip_set.id, sub).is_some(),
                Error::<T>::SubAssetNotFound
            );
        }

        // Actually burn tokens
        Pallet::<T>::internal_burn(target.clone(), (ip_set.id, sub_token), amount)?;

        Self::deposit_event(Event::Burned {
            token: (ip_set.id, sub_token),
            target,
            amount,
        });

        Ok(())
    }

    /// Initiates a multisig transaction. If `caller` has enough votes, execute `call` immediately, otherwise a vote begins.
    pub(crate) fn inner_operate_multisig(
        caller: OriginFor<T>,
        include_caller: bool,
        ipt_id: (T::IpId, Option<T::IpId>),
        metadata: Option<Vec<u8>>,
        call: Box<<T as pallet::Config>::Call>,
    ) -> DispatchResultWithPostInfo {
        let owner = ensure_signed(caller)?;

        // These extrinsics must be called only through InvArch functions or storage will become out of sync
        ensure!(
            !matches!(
                call.get_call_metadata(),
                CallMetadata {
                    pallet_name: "RmrkCore",
                    function_name: "send"
                        | "burn_nft"
                        | "destroy_collection"
                        | "change_collection_issuer",
                } | CallMetadata {
                    pallet_name: "Ipf",
                    function_name: "burn"
                }
            ),
            Error::<T>::CantExecuteThisCall
        );

        // Get IPS/IPT info
        let ipt = IpStorage::<T>::get(ipt_id.0).ok_or(Error::<T>::IpDoesntExist)?;

        let bounded_metadata: Option<BoundedVec<u8, T::MaxMetadata>> = if let Some(vec) = metadata {
            Some(
                vec.try_into()
                    .map_err(|_| Error::<T>::MaxMetadataExceeded)?,
            )
        } else {
            None
        };

        // Get total IP Set token issuance (IPT0 + all sub tokens), weight adjusted (meaning `ZeroPoint(0)` tokens count for 0)
        let total_issuance = ipt.supply
            + SubAssets::<T>::iter_prefix_values(ipt_id.0)
                .map(|sub_asset| {
                    let supply =
                        Balance::<T>::iter_prefix_values((ipt_id.0, Some(sub_asset.id))).sum();

                    // Take into account that some sub tokens have full weight while others may have partial weight or none at all
                    if let OneOrPercent::ZeroPoint(weight) =
                        Pallet::<T>::asset_weight(ipt_id.0, sub_asset.id)?
                    {
                        Some(weight * supply)
                    } else {
                        Some(supply)
                    }
                })
                .collect::<Option<Vec<<T as pallet::Config>::Balance>>>()
                .ok_or(Error::<T>::IpDoesntExist)?
                .into_iter()
                .sum();

        // Get minimum # of votes (tokens w/non-zero weight) required to execute a multisig call
        let total_per_threshold: <T as pallet::Config>::Balance =
            if let OneOrPercent::ZeroPoint(percent) =
                Pallet::<T>::execution_threshold(ipt_id.0).ok_or(Error::<T>::IpDoesntExist)?
            {
                percent * total_issuance
            } else {
                total_issuance
            };

        // Get call metadata
        let call_metadata: [u8; 2] = call
            .encode()
            .split_at(2)
            .0
            .try_into()
            .map_err(|_| Error::<T>::CallHasTooFewBytes)?;

        // Get caller balance of `ipt_id` token, weight adjusted
        let owner_balance: <T as Config>::Balance = if let OneOrPercent::ZeroPoint(percent) = {
            // Function called with some sub token
            if let Some(sub_asset) = ipt_id.1 {
                ensure!(
                    Pallet::<T>::has_permission(ipt_id.0, sub_asset, call_metadata,)?,
                    Error::<T>::SubAssetHasNoPermission
                );

                Pallet::<T>::asset_weight(ipt_id.0, sub_asset).ok_or(Error::<T>::IpDoesntExist)?
            } else {
                // Function called with IPT0 token
                OneOrPercent::One
            }
        } {
            // `ZeroPoint` sub token, so apply asset weight to caller balance
            percent * Balance::<T>::get(ipt_id, owner.clone()).ok_or(Error::<T>::NoPermission)?
        } else {
            // Either IPT0 token or 100% asset weight sub token
            Balance::<T>::get(ipt_id, owner.clone()).ok_or(Error::<T>::NoPermission)?
        };

        let opaque_call: OpaqueCall<T> = WrapperKeepOpaque::from_encoded(call.encode());

        // Compute the `call` hash
        let call_hash: [u8; 32] = blake2_256(&call.encode());

        // Ensure that this exact `call` has not been executed before???
        ensure!(
            Multisig::<T>::get(ipt_id.0, call_hash).is_none(),
            Error::<T>::MultisigOperationAlreadyExists
        );

        // If `caller` has enough balance to meet/exeed the threshold, then go ahead and execute the `call` now.
        if owner_balance >= total_per_threshold {
            // Actually dispatch this call and return the result of it
            let dispatch_result = crate::dispatch::dispatch_call::<T>(
                ipt_id.0,
                if include_caller {
                    Some(owner.clone())
                } else {
                    None
                },
                *call,
            );

            Self::deposit_event(Event::MultisigExecuted {
                ips_id: ipt_id.0,
                executor_account: derive_ips_account::<
                    T,
                    <T as Config>::IpId,
                    <T as frame_system::Config>::AccountId,
                >(
                    ipt_id.0, if include_caller { Some(&owner) } else { None }
                ),
                voter: owner,
                call_hash,
                call: opaque_call,
                result: dispatch_result.map(|_| ()).map_err(|e| e.error),
            });
        } else {
            // Multisig call is now in the voting stage, so update storage.
            Multisig::<T>::insert(
                ipt_id.0,
                call_hash,
                MultisigOperation {
                    signers: vec![(owner.clone(), ipt_id.1)]
                        .try_into()
                        .map_err(|_| Error::<T>::TooManySignatories)?,
                    include_original_caller: include_caller,
                    original_caller: owner.clone(),
                    actual_call: opaque_call.clone(),
                    call_metadata,
                    call_weight: call.get_dispatch_info().weight,
                    metadata: bounded_metadata,
                },
            );

            Self::deposit_event(Event::MultisigVoteStarted {
                ips_id: ipt_id.0,
                executor_account: derive_ips_account::<
                    T,
                    <T as Config>::IpId,
                    <T as frame_system::Config>::AccountId,
                >(
                    ipt_id.0, if include_caller { Some(&owner) } else { None }
                ),
                voter: owner,
                votes_added: owner_balance,
                votes_required: total_per_threshold,
                call_hash,
                call: opaque_call,
            });
        }

        Ok(().into())
    }

    /// Vote on a multisig transaction that has not been executed yet
    pub(crate) fn inner_vote_multisig(
        caller: OriginFor<T>,
        ipt_id: (T::IpId, Option<T::IpId>),
        call_hash: [u8; 32],
    ) -> DispatchResultWithPostInfo {
        Multisig::<T>::try_mutate_exists(ipt_id.0, call_hash, |data| {
            let owner = ensure_signed(caller.clone())?;

            let ipt = IpStorage::<T>::get(ipt_id.0).ok_or(Error::<T>::IpDoesntExist)?;

            let mut old_data = data
                .take()
                .ok_or(Error::<T>::MultisigOperationUninitialized)?;

            // Get caller balance of `ipt_id` token, weight adjusted
            let voter_balance = if let OneOrPercent::ZeroPoint(percent) = {
                // Function called with some sub token
                if let Some(sub_asset) = ipt_id.1 {
                    ensure!(
                        Pallet::<T>::has_permission(ipt_id.0, sub_asset, old_data.call_metadata,)?,
                        Error::<T>::SubAssetHasNoPermission
                    );

                    Pallet::<T>::asset_weight(ipt_id.0, sub_asset)
                        .ok_or(Error::<T>::IpDoesntExist)?
                } else {
                    // Function called with IPT0 token
                    OneOrPercent::One
                }
            } {
                percent
                    * Balance::<T>::get(ipt_id, owner.clone()).ok_or(Error::<T>::NoPermission)?
            } else {
                Balance::<T>::get(ipt_id, owner.clone()).ok_or(Error::<T>::NoPermission)?
            };

            // Get total # of votes cast so far towards this multisig call
            let total_in_operation: <T as pallet::Config>::Balance = old_data
                .signers
                .clone()
                .into_iter()
                .map(|(voter, asset): (T::AccountId, Option<T::IpId>)| {
                    Balance::<T>::get((ipt_id.0, asset), voter).map(|balance| {
                        if let OneOrPercent::ZeroPoint(percent) = if let Some(sub_asset) = asset {
                            Pallet::<T>::asset_weight(ipt_id.0, sub_asset).unwrap()
                        } else {
                            OneOrPercent::One
                        } {
                            percent * balance
                        } else {
                            balance
                        }
                    })
                })
                .collect::<Option<Vec<<T as pallet::Config>::Balance>>>()
                .ok_or(Error::<T>::NoPermission)?
                .into_iter()
                .sum();

            // Get total IP Set token issuance (IPT0 + all sub tokens), weight adjusted (meaning `ZeroPoint(0)` tokens count for 0)
            let total_issuance = ipt.supply
                + SubAssets::<T>::iter_prefix_values(ipt_id.0)
                    .map(|sub_asset| {
                        let supply =
                            Balance::<T>::iter_prefix_values((ipt_id.0, Some(sub_asset.id))).sum();

                        if let OneOrPercent::ZeroPoint(weight) =
                            Pallet::<T>::asset_weight(ipt_id.0, sub_asset.id)?
                        {
                            Some(weight * supply)
                        } else {
                            Some(supply)
                        }
                    })
                    .collect::<Option<Vec<<T as pallet::Config>::Balance>>>()
                    .ok_or(Error::<T>::IpDoesntExist)?
                    .into_iter()
                    .sum();

            // Get minimum # of votes (tokens w/non-zero weight) required to execute a multisig call.
            let total_per_threshold: <T as pallet::Config>::Balance =
                if let OneOrPercent::ZeroPoint(percent) =
                    Pallet::<T>::execution_threshold(ipt_id.0).ok_or(Error::<T>::IpDoesntExist)?
                {
                    percent * total_issuance
                } else {
                    total_issuance
                };

            // If already cast votes + `caller` weighted votes are enough to meet/exeed the threshold, then go ahead and execute the `call` now.
            if (total_in_operation + voter_balance) >= total_per_threshold {
                // Multisig storage records are removed when the transaction is executed or the vote on the transaction is withdrawn
                *data = None;

                // Actually dispatch this call and return the result of it
                let dispatch_result = crate::dispatch::dispatch_call::<T>(
                    ipt_id.0,
                    if old_data.include_original_caller {
                        Some(old_data.original_caller.clone())
                    } else {
                        None
                    },
                    old_data
                        .actual_call
                        .try_decode()
                        .ok_or(Error::<T>::CouldntDecodeCall)?,
                );
                Self::deposit_event(Event::MultisigExecuted {
                    ips_id: ipt_id.0,
                    executor_account: derive_ips_account::<
                        T,
                        <T as Config>::IpId,
                        <T as frame_system::Config>::AccountId,
                    >(
                        ipt_id.0,
                        if old_data.include_original_caller {
                            Some(&old_data.original_caller)
                        } else {
                            None
                        },
                    ),
                    voter: owner,
                    call_hash,
                    call: old_data.actual_call,
                    result: dispatch_result.map(|_| ()).map_err(|e| e.error),
                });
            } else {
                // `caller`s votes were not enough to pass the vote

                // Update storage
                old_data.signers = {
                    let mut v = old_data.signers.to_vec();
                    v.push((owner.clone(), ipt_id.1));
                    v.try_into().map_err(|_| Error::<T>::MaxMetadataExceeded)?
                };
                *data = Some(old_data.clone());

                Self::deposit_event(Event::MultisigVoteAdded {
                    ips_id: ipt_id.0,
                    executor_account: derive_ips_account::<
                        T,
                        <T as Config>::IpId,
                        <T as frame_system::Config>::AccountId,
                    >(
                        ipt_id.0,
                        if old_data.include_original_caller {
                            Some(&old_data.original_caller)
                        } else {
                            None
                        },
                    ),
                    voter: owner,
                    votes_added: voter_balance,
                    current_votes: (total_in_operation + voter_balance),
                    votes_required: total_per_threshold,
                    call_hash,
                    call: old_data.actual_call,
                });
            }

            Ok(().into())
        })
    }

    /// Withdraw vote from an ongoing multisig operation
    pub(crate) fn inner_withdraw_vote_multisig(
        caller: OriginFor<T>,
        ipt_id: (T::IpId, Option<T::IpId>),
        call_hash: [u8; 32],
    ) -> DispatchResultWithPostInfo {
        Multisig::<T>::try_mutate_exists(ipt_id.0, call_hash, |data| {
            let owner = ensure_signed(caller.clone())?;

            let ipt = IpStorage::<T>::get(ipt_id.0).ok_or(Error::<T>::IpDoesntExist)?;

            let mut old_data = data
                .take()
                .ok_or(Error::<T>::MultisigOperationUninitialized)?;

            // Can only withdraw your vote if you have already voted on this multisig operation
            ensure!(
                old_data.signers.iter().any(|signer| signer.0 == owner),
                Error::<T>::NotAVoter
            );

            // if `caller` is the account who created this vote, they can dissolve it immediately
            if owner == old_data.original_caller {
                // Multisig storage records are removed when the transaction is executed or the vote on the transaction is withdrawn
                *data = None;

                Self::deposit_event(Event::MultisigCanceled {
                    ips_id: ipt_id.0,
                    executor_account: derive_ips_account::<
                        T,
                        <T as Config>::IpId,
                        <T as frame_system::Config>::AccountId,
                    >(
                        ipt_id.0,
                        if old_data.include_original_caller {
                            Some(&old_data.original_caller)
                        } else {
                            None
                        },
                    ),
                    call_hash,
                });
            } else {
                // caller is not the creator of this vote
                // Get caller balance of `ipt_id` token, weight adjusted
                let voter_balance = if let OneOrPercent::ZeroPoint(percent) = {
                    if let Some(sub_asset) = ipt_id.1 {
                        Pallet::<T>::asset_weight(ipt_id.0, sub_asset)
                            .ok_or(Error::<T>::IpDoesntExist)?
                    } else {
                        OneOrPercent::One
                    }
                } {
                    percent
                        * Balance::<T>::get(ipt_id, owner.clone())
                            .ok_or(Error::<T>::NoPermission)?
                } else {
                    Balance::<T>::get(ipt_id, owner.clone()).ok_or(Error::<T>::NoPermission)?
                };

                // Get total IP Set token issuance (IPT0 + all sub tokens), weight adjusted (meaning `ZeroPoint(0)` tokens count for 0)
                let total_issuance = ipt.supply
                    + SubAssets::<T>::iter_prefix_values(ipt_id.0)
                        .map(|sub_asset| {
                            let supply =
                                Balance::<T>::iter_prefix_values((ipt_id.0, Some(sub_asset.id)))
                                    .sum();

                            if let OneOrPercent::ZeroPoint(weight) =
                                Pallet::<T>::asset_weight(ipt_id.0, sub_asset.id)?
                            {
                                Some(weight * supply)
                            } else {
                                Some(supply)
                            }
                        })
                        .collect::<Option<Vec<<T as pallet::Config>::Balance>>>()
                        .ok_or(Error::<T>::IpDoesntExist)?
                        .into_iter()
                        .sum();

                // Get minimum # of votes (tokens w/non-zero weight) required to execute a multisig call
                let total_per_threshold: <T as pallet::Config>::Balance =
                    if let OneOrPercent::ZeroPoint(percent) =
                        Pallet::<T>::execution_threshold(ipt_id.0)
                            .ok_or(Error::<T>::IpDoesntExist)?
                    {
                        percent * total_issuance
                    } else {
                        total_issuance
                    };

                // Remove caller from the list of signers
                old_data.signers = old_data
                    .signers
                    .into_iter()
                    .filter(|signer| signer.0 != owner)
                    .collect::<Vec<(T::AccountId, Option<T::IpId>)>>()
                    .try_into()
                    .map_err(|_| Error::<T>::TooManySignatories)?;

                *data = Some(old_data.clone());

                Self::deposit_event(Event::MultisigVoteWithdrawn {
                    ips_id: ipt_id.0,
                    executor_account: derive_ips_account::<
                        T,
                        <T as Config>::IpId,
                        <T as frame_system::Config>::AccountId,
                    >(
                        ipt_id.0,
                        if old_data.include_original_caller {
                            Some(&old_data.original_caller)
                        } else {
                            None
                        },
                    ),
                    voter: owner,
                    votes_removed: voter_balance,
                    votes_required: total_per_threshold,
                    call_hash,
                    call: old_data.actual_call,
                });
            }

            Ok(().into())
        })
    }

    /// Create one or more sub tokens for an IP Set
    pub(crate) fn inner_create_sub_token(
        origin: OriginFor<T>,
        sub_tokens: SubAssetsWithEndowment<T>,
    ) -> DispatchResultWithPostInfo {
        let ip_set = ensure_multisig::<T, OriginFor<T>>(origin)?;

        IpStorage::<T>::try_mutate_exists(ip_set.id, |ips| -> DispatchResultWithPostInfo {
            let old_ips = ips.clone().ok_or(Error::<T>::IpDoesntExist)?;

            // Can only create sub tokens from the topmost parent, an IP Set that is `Parentage::Parent`.
            // Additionally, call must be from IP Set multisig
            match old_ips.parentage {
                Parentage::Parent(ips_account) => {
                    ensure!(
                        ips_account
                            == derive_ips_account::<
                                T,
                                <T as pallet::Config>::IpId,
                                <T as frame_system::Config>::AccountId,
                            >(ip_set.id, None),
                        Error::<T>::NoPermission
                    )
                }
                Parentage::Child(..) => return Err(Error::<T>::NotParent.into()),
            }

            // Create sub tokens, if none already exist
            for sub in sub_tokens.clone() {
                ensure!(
                    !SubAssets::<T>::contains_key(ip_set.id, sub.0.id),
                    Error::<T>::SubAssetAlreadyExists
                );

                SubAssets::<T>::insert(ip_set.id, sub.0.id, &sub.0);

                Balance::<T>::insert((ip_set.id, Some(sub.0.id)), sub.1 .0, sub.1 .1);
            }

            Self::deposit_event(Event::SubTokenCreated {
                sub_tokens_with_endowment: sub_tokens
                    .into_iter()
                    .map(|sub| ((ip_set.id, sub.0.id), sub.1 .0, sub.1 .1))
                    .collect(),
            });

            Ok(().into())
        })
    }

    /// Mint `amount` of specified token to `target` account
    pub fn internal_mint(
        ipt_id: (T::IpId, Option<T::IpId>),
        target: T::AccountId,
        amount: <T as pallet::Config>::Balance,
    ) -> DispatchResult {
        IpStorage::<T>::try_mutate(ipt_id.0, |ipt| -> DispatchResult {
            Balance::<T>::try_mutate(ipt_id, target, |balance| -> DispatchResult {
                let old_balance = balance.take().unwrap_or_default();
                // Increase `target` account's balance of `ipt_id` sub token by `amount`
                *balance = Some(
                    old_balance
                        .checked_add(&amount)
                        .ok_or(Error::<T>::Overflow)?,
                );

                let mut old_ipt = ipt.take().ok_or(Error::<T>::IpDoesntExist)?;

                // If minting IPT0 tokens, update supply
                if ipt_id.1.is_none() {
                    old_ipt.supply = old_ipt
                        .supply
                        .checked_add(&amount)
                        .ok_or(Error::<T>::Overflow)?;
                }

                *ipt = Some(old_ipt);

                Ok(())
            })
        })
    }

    /// Burn `amount` of specified token from `target` account
    pub fn internal_burn(
        target: T::AccountId,
        ipt_id: (T::IpId, Option<T::IpId>),
        amount: <T as pallet::Config>::Balance,
    ) -> DispatchResult {
        IpStorage::<T>::try_mutate(ipt_id.0, |ipt| -> DispatchResult {
            Balance::<T>::try_mutate(ipt_id, target, |balance| -> DispatchResult {
                let old_balance = balance.take().ok_or(Error::<T>::IpDoesntExist)?;
                // Decrease `target` account's balance of `ipt_id` sub token by `amount`
                *balance = Some(
                    old_balance
                        .checked_sub(&amount)
                        .ok_or(Error::<T>::NotEnoughAmount)?,
                );

                let mut old_ipt = ipt.take().ok_or(Error::<T>::IpDoesntExist)?;

                // If burning IPT0 tokens, update supply
                if ipt_id.1.is_none() {
                    old_ipt.supply = old_ipt
                        .supply
                        .checked_sub(&amount)
                        .ok_or(Error::<T>::NotEnoughAmount)?;
                }

                *ipt = Some(old_ipt);

                Ok(())
            })
        })
    }
}
