use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;

use bincode;
use crypto::Signature;
use serde::{Deserialize, Serialize};

use super::{Change, ErrorKind, Result};
use fault_log::{FaultKind, FaultLog};
use messaging::NetworkInfo;

/// A buffer and counter collecting pending and committed votes for validator set changes.
///
/// This is reset whenever the set of validators changes or a change reaches _f + 1_ votes. We call
/// the epochs since the last reset the current _era_.
#[derive(Debug)]
pub struct VoteCounter<NodeUid> {
    /// Shared network data.
    netinfo: Arc<NetworkInfo<NodeUid>>,
    /// The epoch when voting was reset.
    era: u64,
    /// Pending node transactions that we will propose in the next epoch.
    pending: BTreeMap<NodeUid, SignedVote<NodeUid>>,
    /// Collected votes for adding or removing nodes. Each node has one vote, and casting another
    /// vote revokes the previous one.
    committed: BTreeMap<NodeUid, Vote<NodeUid>>,
}

impl<NodeUid> VoteCounter<NodeUid>
where
    NodeUid: Eq + Hash + Ord + Clone + Debug + Serialize + for<'r> Deserialize<'r>,
{
    /// Creates a new `VoteCounter` object with empty buffer and counter.
    pub fn new(netinfo: Arc<NetworkInfo<NodeUid>>, era: u64) -> Self {
        VoteCounter {
            era,
            netinfo,
            pending: BTreeMap::new(),
            committed: BTreeMap::new(),
        }
    }

    /// Creates a signed vote for the given change, and inserts it into the pending votes buffer.
    pub fn sign_vote_for(&mut self, change: Change<NodeUid>) -> Result<&SignedVote<NodeUid>> {
        let voter = self.netinfo.our_uid().clone();
        let vote = Vote {
            change,
            era: self.era,
            num: self.pending.get(&voter).map_or(0, |sv| sv.vote.num + 1),
        };
        let ser_vote =
            bincode::serialize(&vote).map_err(|err| ErrorKind::SignVoteForBincode(*err))?;
        let signed_vote = SignedVote {
            vote,
            voter: voter.clone(),
            sig: self.netinfo.secret_key().sign(ser_vote),
        };
        self.pending.insert(voter.clone(), signed_vote);
        Ok(self.pending.get(&voter).expect("entry was just inserted"))
    }

    /// Inserts a pending vote into the buffer, if it has a higher number than the existing one.
    pub fn add_pending_vote(
        &mut self,
        sender_id: &NodeUid,
        signed_vote: SignedVote<NodeUid>,
    ) -> Result<FaultLog<NodeUid>> {
        if signed_vote.vote.era != self.era
            || self
                .pending
                .get(&signed_vote.voter)
                .map_or(false, |sv| sv.vote.num >= signed_vote.vote.num)
        {
            return Ok(FaultLog::new()); // The vote is obsolete or already exists.
        }
        if !self.validate(&signed_vote)? {
            return Ok(FaultLog::init(
                sender_id.clone(),
                FaultKind::InvalidVoteSignature,
            ));
        }
        self.pending.insert(signed_vote.voter.clone(), signed_vote);
        Ok(FaultLog::new())
    }

    /// Returns an iterator over all pending votes that are newer than their voter's committed
    /// vote.
    pub fn pending_votes(&self) -> impl Iterator<Item = &SignedVote<NodeUid>> {
        self.pending.values().filter(move |signed_vote| {
            self.committed
                .get(&signed_vote.voter)
                .map_or(true, |vote| vote.num < signed_vote.vote.num)
        })
    }

    // TODO: Document and return fault logs?
    pub fn add_committed_votes<I>(
        &mut self,
        proposer_id: &NodeUid,
        signed_votes: I,
    ) -> Result<FaultLog<NodeUid>>
    where
        I: IntoIterator<Item = SignedVote<NodeUid>>,
    {
        let mut fault_log = FaultLog::new();
        for signed_vote in signed_votes {
            fault_log.extend(self.add_committed_vote(proposer_id, signed_vote)?);
        }
        Ok(fault_log)
    }

    /// Inserts a committed vote into the counter, if it has a higher number than the existing one.
    pub fn add_committed_vote(
        &mut self,
        proposer_id: &NodeUid,
        signed_vote: SignedVote<NodeUid>,
    ) -> Result<FaultLog<NodeUid>> {
        if self
            .committed
            .get(&signed_vote.voter)
            .map_or(false, |vote| vote.num >= signed_vote.vote.num)
        {
            return Ok(FaultLog::new()); // The vote is obsolete or already exists.
        }
        if signed_vote.vote.era != self.era || !self.validate(&signed_vote)? {
            return Ok(FaultLog::init(
                proposer_id.clone(),
                FaultKind::InvalidCommittedVote,
            ));
        }
        self.committed.insert(signed_vote.voter, signed_vote.vote);
        Ok(FaultLog::new())
    }

    /// Returns the change that has at least _f + 1_ votes, if any.
    pub fn compute_winner(&self) -> Option<&Change<NodeUid>> {
        let mut vote_counts: HashMap<&Change<NodeUid>, usize> = HashMap::new();
        for vote in self.committed.values() {
            let change = &vote.change;
            let entry = vote_counts.entry(change).or_insert(0);
            *entry += 1;
            if *entry > self.netinfo.num_faulty() {
                return Some(change);
            }
        }
        None
    }

    /// Returns `true` if the signature is valid.
    fn validate(&self, signed_vote: &SignedVote<NodeUid>) -> Result<bool> {
        let ser_vote =
            bincode::serialize(&signed_vote.vote).map_err(|err| ErrorKind::ValidateBincode(*err))?;
        let pk_opt = self.netinfo.public_key(&signed_vote.voter);
        Ok(pk_opt.map_or(false, |pk| pk.verify(&signed_vote.sig, ser_vote)))
    }
}

/// A vote fore removing or adding a validator.
#[derive(Eq, PartialEq, Debug, Serialize, Deserialize, Hash, Clone)]
struct Vote<NodeUid> {
    /// The change this vote is for.
    change: Change<NodeUid>,
    /// The epoch in which the current era began.
    era: u64,
    /// The vote number: VoteCounter can be changed by casting another vote with a higher number.
    num: u64,
}

/// A signed vote for removing or adding a validator.
#[derive(Eq, PartialEq, Debug, Serialize, Deserialize, Hash, Clone)]
pub struct SignedVote<NodeUid> {
    vote: Vote<NodeUid>,
    voter: NodeUid,
    sig: Signature,
}

impl<NodeUid> SignedVote<NodeUid> {
    pub fn era(&self) -> u64 {
        self.vote.era
    }

    pub fn voter(&self) -> &NodeUid {
        &self.voter
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Change, SignedVote, VoteCounter};
    use fault_log::{FaultKind, FaultLog};
    use messaging::NetworkInfo;

    /// Returns a vector of `node_num` `VoteCounter`s, and some signed example votes.
    ///
    /// If `signed_votes` is the second entry of the return value, then `signed_votes[i][j]` is the
    /// the vote for `Remove(j)` by node `i`. Each node signed `Remove(0)`, `Remove(1)`, ... in
    /// order.
    fn setup(node_num: usize, era: u64) -> (Vec<VoteCounter<usize>>, Vec<Vec<SignedVote<usize>>>) {
        // Create keys for threshold cryptography.
        let netinfos = NetworkInfo::generate_map(0..node_num);

        // Create a `VoteCounter` instance for each node.
        let create_counter =
            |(_, netinfo): (_, NetworkInfo<_>)| VoteCounter::new(Arc::new(netinfo), era);
        let mut counters: Vec<_> = netinfos.into_iter().map(create_counter).collect();

        // Sign a few votes.
        let sign_votes = |counter: &mut VoteCounter<usize>| {
            (0..node_num)
                .map(Change::Remove)
                .map(|change| counter.sign_vote_for(change).expect("sign vote").clone())
                .collect::<Vec<_>>()
        };
        let signed_votes: Vec<_> = counters.iter_mut().map(sign_votes).collect();
        (counters, signed_votes)
    }

    #[test]
    fn test_pending_votes() {
        let node_num = 4;
        let era = 5;
        // Create the counter instances and the matrix of signed votes.
        let (mut counters, sv) = setup(node_num, era);
        // We will only use counter number 0.
        let ct = &mut counters[0];

        // Node 0 already contains its own vote for `Remove(3)`. Add two more.
        let faults = ct
            .add_pending_vote(&1, sv[1][2].clone())
            .expect("add pending");
        assert!(faults.is_empty());
        let faults = ct
            .add_pending_vote(&2, sv[2][1].clone())
            .expect("add pending");
        assert!(faults.is_empty());
        // Include a vote with a wrong signature.
        let fake_vote = SignedVote {
            sig: sv[2][1].sig.clone(),
            ..sv[3][1].clone()
        };
        let faults = ct.add_pending_vote(&1, fake_vote).expect("add pending");
        let expected_faults = FaultLog::init(1, FaultKind::InvalidVoteSignature);
        assert_eq!(faults, expected_faults);
        assert_eq!(
            ct.pending_votes().collect::<Vec<_>>(),
            vec![&sv[0][3], &sv[1][2], &sv[2][1]]
        );

        // Now add an older vote by node 1 and a newer one by node 2. Only the latter should be
        // included.
        let faults = ct
            .add_pending_vote(&3, sv[1][1].clone())
            .expect("add pending");
        assert!(faults.is_empty());
        let faults = ct
            .add_pending_vote(&1, sv[2][2].clone())
            .expect("add pending");
        assert!(faults.is_empty());
        assert_eq!(
            ct.pending_votes().collect::<Vec<_>>(),
            vec![&sv[0][3], &sv[1][2], &sv[2][2]]
        );

        // Adding a committed vote removes it from the pending ones, unless it is older.
        let vote_batch = vec![sv[1][3].clone(), sv[2][1].clone(), sv[0][3].clone()];
        ct.add_committed_votes(&1, vote_batch)
            .expect("add committed");
        assert_eq!(ct.pending_votes().collect::<Vec<_>>(), vec![&sv[2][2]]);
    }

    #[test]
    fn test_committed_votes() {
        let node_num = 4; // At most one faulty node.
        let era = 5;
        // Create the counter instances and the matrix of signed votes.
        let (mut counters, sv) = setup(node_num, era);
        // We will only use counter number 0.
        let ct = &mut counters[0];

        let mut vote_batch = vec![sv[1][1].clone()];
        // Include a vote with a wrong signature.
        vote_batch.push(SignedVote {
            sig: sv[2][1].sig.clone(),
            ..sv[3][1].clone()
        });
        let faults = ct
            .add_committed_votes(&1, vote_batch)
            .expect("add committed");
        let expected_faults = FaultLog::init(1, FaultKind::InvalidCommittedVote);
        assert_eq!(faults, expected_faults);
        assert_eq!(ct.compute_winner(), None);

        // Adding the second vote for `Remove(1)` should return the change: It has f + 1 votes.
        let faults = ct
            .add_committed_vote(&1, sv[2][1].clone())
            .expect("add committed");
        assert!(faults.is_empty());
        assert_eq!(ct.compute_winner(), Some(&Change::Remove(1)));
    }
}