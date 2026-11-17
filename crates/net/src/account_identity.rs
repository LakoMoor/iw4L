use std::{fmt, sync::Arc};

use master_protocol::MemberId;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use sim::AccountId;

use crate::ConnectionId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountIdentityError {
    Entropy,
    InvalidKey,
    InvalidContext,
    InvalidProof,
}

struct KeyMaterial {
    seed: [u8; 32],
    pair: SigningKey,
}

#[derive(Clone)]
pub struct AccountKey(Arc<KeyMaterial>);

impl fmt::Debug for AccountKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountKey")
            .field("account", &self.account())
            .finish_non_exhaustive()
    }
}

impl AccountKey {
    pub fn generate() -> Result<Self, AccountIdentityError> {
        let mut seed = [0; 32];
        while seed == [0; 32] {
            getrandom::fill(&mut seed)
                .map_err(|_| AccountIdentityError::Entropy)?;
        }
        Self::from_seed(seed)
    }

    pub fn from_seed(seed: [u8; 32]) -> Result<Self, AccountIdentityError> {
        if seed == [0; 32] {
            return Err(AccountIdentityError::InvalidKey);
        }
        let pair = SigningKey::from_bytes(&seed);
        Ok(Self(Arc::new(KeyMaterial { seed, pair })))
    }

    pub fn export_seed(&self) -> [u8; 32] {
        self.0.seed
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.0.pair.verifying_key().to_bytes()
    }

    pub fn account(&self) -> AccountId {
        account_from_public_key(&self.public_key())
    }

    pub fn prove(&self, challenge: &AccountChallenge, payload: &[u8]) -> AccountProof {
        let account = self.account();
        AccountProof {
            account,
            public_key: self.public_key(),
            signature: self
                .0
                .pair
                .sign(&proof_message(challenge, account, payload))
                .to_bytes(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountChallenge {
    pub match_key: frame::MatchKey,
    pub connection: ConnectionId,
    pub member: MemberId,
    pub nonce: [u8; 32],
}

impl AccountChallenge {
    pub fn new(
        match_key: frame::MatchKey,
        connection: ConnectionId,
        member: MemberId,
    ) -> Result<Self, AccountIdentityError> {
        if match_key.is_none()
            || match_key.session_id == [0; 16]
            || connection.0 == 0
            || member.0 == [0; 16]
        {
            return Err(AccountIdentityError::InvalidContext);
        }
        let mut nonce = [0; 32];
        while nonce == [0; 32] {
            getrandom::fill(&mut nonce)
                .map_err(|_| AccountIdentityError::Entropy)?;
        }
        Ok(Self {
            match_key,
            connection,
            member,
            nonce,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountProof {
    pub account: AccountId,
    pub public_key: [u8; 32],
    pub signature: [u8; 64],
}

impl AccountProof {
    pub fn verify(
        &self,
        challenge: &AccountChallenge,
        payload: &[u8],
    ) -> Result<AccountId, AccountIdentityError> {
        if self.account.0 == [0; 16] || account_from_public_key(&self.public_key) != self.account {
            return Err(AccountIdentityError::InvalidProof);
        }
        VerifyingKey::from_bytes(&self.public_key)
            .and_then(|key| {
                key.verify(
                    &proof_message(challenge, self.account, payload),
                    &Signature::from_bytes(&self.signature),
                )
            })
            .map_err(|_| AccountIdentityError::InvalidProof)?;
        Ok(self.account)
    }
}

fn account_from_public_key(public_key: &[u8; 32]) -> AccountId {
    let mut hash = Sha256::new();
    hash.update(b"IW4L account identity v1\0");
    hash.update(public_key);
    AccountId(hash.finalize()[..16].try_into().unwrap())
}

fn proof_message(challenge: &AccountChallenge, account: AccountId, payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(160);
    message.extend_from_slice(b"IW4L account proof v1\0");
    message.extend_from_slice(&challenge.match_key.session_id);
    message.extend_from_slice(&challenge.match_key.match_epoch.to_le_bytes());
    message.extend_from_slice(&challenge.connection.0.to_le_bytes());
    message.extend_from_slice(&challenge.member.0);
    message.extend_from_slice(&challenge.nonce);
    message.extend_from_slice(&account.0);
    message.extend_from_slice(&Sha256::digest(payload));
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Accounts persisted by the `ring`-backed build must keep their identity:
    /// the same seed has to give the RFC 8032 public key.
    #[test]
    fn seed_matches_rfc8032_vector() {
        let seed: [u8; 32] = hex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
            .try_into()
            .unwrap();
        let key = AccountKey::from_seed(seed).unwrap();
        assert_eq!(
            key.public_key().to_vec(),
            hex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
        );
        assert_eq!(key.export_seed(), seed);
    }

    #[test]
    fn proof_round_trips() {
        let key = AccountKey::generate().unwrap();
        let challenge = AccountChallenge::new(
            frame::MatchKey {
                session_id: [7; 16],
                match_epoch: 1,
            },
            ConnectionId(3),
            MemberId([9; 16]),
        )
        .unwrap();
        let proof = key.prove(&challenge, b"payload");
        assert_eq!(proof.verify(&challenge, b"payload"), Ok(key.account()));
        assert!(proof.verify(&challenge, b"other").is_err());
    }
}
