use {
    crate::store::{
        storage::{
            CompletedAccumulatorState,
            MessageState,
        },
        Store,
    },
    anyhow::{
        anyhow,
        Result,
    },
    pythnet_sdk::{
        accumulators::{
            merkle::{
                MerklePath,
                MerkleTree,
            },
            Accumulator,
        },
        hashers::keccak256_160::Keccak160,
        wire::{
            to_vec,
            v1::{
                AccumulatorUpdateData,
                MerklePriceUpdate,
                Proof,
                WormholeMerkleRoot,
            },
        },
    },
};

#[derive(Clone, PartialEq, Debug)]
pub struct WormholeMerkleState {
    pub root: WormholeMerkleRoot,
    pub vaa:  Vec<u8>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct WormholeMerkleMessageProof {
    pub vaa:   Vec<u8>,
    pub proof: MerklePath<Keccak160>,
}

pub async fn store_wormhole_merkle_verified_message(
    store: &Store,
    root: WormholeMerkleRoot,
    vaa_bytes: Vec<u8>,
) -> Result<()> {
    store
        .storage
        .update_accumulator_state(
            root.slot,
            Box::new(|mut state| {
                state.wormhole_merkle_state = Some(WormholeMerkleState {
                    root,
                    vaa: vaa_bytes,
                });
                state
            }),
        )
        .await?;
    Ok(())
}

pub fn construct_message_states_proofs(
    completed_accumulator_state: &CompletedAccumulatorState,
) -> Result<Vec<WormholeMerkleMessageProof>> {
    let accumulator_messages = &completed_accumulator_state.accumulator_messages;
    let wormhole_merkle_state = &completed_accumulator_state.wormhole_merkle_state;

    // Check whether the state is valid
    let merkle_acc = match MerkleTree::<Keccak160>::from_set(
        accumulator_messages.raw_messages.iter().map(|m| m.as_ref()),
    ) {
        Some(merkle_acc) => merkle_acc,
        None => return Ok(vec![]), // It only happens when the message set is empty
    };

    if merkle_acc.root.as_bytes() != wormhole_merkle_state.root.root {
        return Err(anyhow!("Invalid merkle root"));
    }

    accumulator_messages
        .raw_messages
        .iter()
        .map(|m| {
            Ok(WormholeMerkleMessageProof {
                vaa:   wormhole_merkle_state.vaa.clone(),
                proof: merkle_acc
                    .prove(m.as_ref())
                    .ok_or(anyhow!("Failed to prove message"))?,
            })
        })
        .collect::<Result<Vec<WormholeMerkleMessageProof>>>()
}

pub fn construct_update_data(mut message_states: Vec<&MessageState>) -> Result<Vec<Vec<u8>>> {
    message_states.sort_by_key(
        |m| m.proof_set.wormhole_merkle_proof.vaa.clone(), // FIXME: This is not efficient
    );

    message_states
        .group_by(|a, b| {
            a.proof_set.wormhole_merkle_proof.vaa == b.proof_set.wormhole_merkle_proof.vaa
        })
        .map(|messages| {
            let vaa = messages
                .get(0)
                .ok_or(anyhow!("Empty message set"))?
                .proof_set
                .wormhole_merkle_proof
                .vaa
                .clone();

            Ok(to_vec::<_, byteorder::BE>(&AccumulatorUpdateData::new(
                Proof::WormholeMerkle {
                    vaa:     vaa.into(),
                    updates: messages
                        .iter()
                        .map(|message| {
                            Ok(MerklePriceUpdate {
                                message: to_vec::<_, byteorder::BE>(&message.message)
                                    .map_err(|e| anyhow!("Failed to serialize message: {}", e))?
                                    .into(),
                                proof:   message.proof_set.wormhole_merkle_proof.proof.clone(),
                            })
                        })
                        .collect::<Result<_>>()?,
                },
            ))?)
        })
        .collect::<Result<Vec<Vec<u8>>>>()
}
