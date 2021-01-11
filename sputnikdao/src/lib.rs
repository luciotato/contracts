use std::collections::HashMap;

use near_lib::types::{Duration, WrappedBalance, WrappedDuration};
use near_sdk::borsh::{self, BorshDeserialize, BorshSerialize};
use near_sdk::collections::{UnorderedSet, Vector};
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::{env, near_bindgen, AccountId, Balance, Promise};

#[global_allocator]
static ALLOC: near_sdk::wee_alloc::WeeAlloc<'_> = near_sdk::wee_alloc::WeeAlloc::INIT;

const MAX_DESCRIPTION_LENGTH: usize = 280;

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(crate = "near_sdk::serde")]
pub enum Vote {
    Yes,
    No,
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone)]
#[serde(crate = "near_sdk::serde")]
#[serde(untagged)]
pub enum NumOrRatio {
    Number(u64),
    Ratio(u64, u64),
}

/// Policy item, defining how many votes required to approve up to this much amount.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone)]
#[serde(crate = "near_sdk::serde")]
pub struct PolicyItem {
    pub max_amount: WrappedBalance,
    pub votes: NumOrRatio,
}

impl PolicyItem {
    pub fn num_votes(&self, num_council: u64) -> u64 {
        match self.votes {
            NumOrRatio::Number(num_votes) => num_votes,
            NumOrRatio::Ratio(l, r) => std::cmp::min(num_council * l / r + 1, num_council),
        }
    }
}

fn vote_requirement(policy: &[PolicyItem], num_council: u64, amount: Option<Balance>) -> u64 {
    if let Some(amount) = amount {
        // TODO: replace with binary search.
        for item in policy {
            if item.max_amount.0 > amount {
                return item.num_votes(num_council);
            }
        }
    }
    policy[policy.len() - 1].num_votes(num_council)
}

#[derive(BorshSerialize, BorshDeserialize, Eq, PartialEq, Debug, Serialize, Deserialize, Clone)]
#[serde(crate = "near_sdk::serde")]
pub enum ProposalStatus {
    /// Proposal is in active voting stage.
    Vote,
    /// Proposal has successfully passed.
    Success,
    /// Proposal was rejected by the vote.
    Reject,
    /// Vote for proposal has failed due (not enuough votes).
    Fail,
    /// Given voting policy, the uncontested minimum of votes was acquired.
    /// Delaying the finalization of the proposal to check that there is no contenders (who would vote against).
    Delay,
}

impl ProposalStatus {
    pub fn is_finalized(&self) -> bool {
        self != &ProposalStatus::Vote && self != &ProposalStatus::Delay
    }
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(crate = "near_sdk::serde")]
#[serde(tag = "type")]
pub enum ProposalKind {
    NewCouncil,
    RemoveCouncil,
    Payout { amount: WrappedBalance },
    ChangeVotePeriod { vote_period: WrappedDuration },
    ChangeBond { bond: WrappedBalance },
    ChangePolicy { policy: Vec<PolicyItem> },
    ChangePurpose { purpose: String },
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[serde(crate = "near_sdk::serde")]
pub struct Proposal {
    status: ProposalStatus,
    proposer: AccountId,
    target: AccountId,
    description: String,
    kind: ProposalKind,
    vote_period_end: Duration,
    vote_yes: u64,
    vote_no: u64,
    votes: HashMap<AccountId, Vote>,
}

impl Proposal {
    pub fn get_amount(&self) -> Option<Balance> {
        match self.kind {
            ProposalKind::Payout { amount } => Some(amount.0),
            _ => None,
        }
    }

    /// Compute new vote status given council size and current timestamp.
    pub fn vote_status(&self, policy: &[PolicyItem], num_council: u64) -> ProposalStatus {
        let votes_required = vote_requirement(policy, num_council, self.get_amount());
        let max_votes = policy[policy.len() - 1].num_votes(num_council);
        if self.vote_yes >= max_votes {
            ProposalStatus::Success
        } else if self.vote_yes >= votes_required && self.vote_no == 0 {
            if env::block_timestamp() > self.vote_period_end {
                ProposalStatus::Success
            } else {
                ProposalStatus::Delay
            }
        } else if self.vote_no >= max_votes {
            ProposalStatus::Reject
        } else if env::block_timestamp() > self.vote_period_end
            || self.vote_yes + self.vote_no == num_council
        {
            ProposalStatus::Fail
        } else {
            ProposalStatus::Vote
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "near_sdk::serde")]
pub struct ProposalInput {
    target: AccountId,
    description: String,
    kind: ProposalKind,
}

#[near_bindgen]
#[derive(BorshSerialize, BorshDeserialize)]
pub struct SputnikDAO {
    purpose: String,
    bond: Balance,
    vote_period: Duration,
    grace_period: Duration,
    policy: Vec<PolicyItem>,
    council: UnorderedSet<AccountId>,
    proposals: Vector<Proposal>,
}

impl Default for SputnikDAO {
    fn default() -> Self {
        env::panic(b"SputnikDAO should be initialized before usage")
    }
}

#[near_bindgen]
impl SputnikDAO {
    #[init]
    pub fn new(
        purpose: String,
        council: Vec<AccountId>,
        bond: WrappedBalance,
        vote_period: WrappedDuration,
        grace_period: WrappedDuration,
    ) -> Self {
        assert!(!env::state_exists(), "The contract is already initialized");
        let mut dao = Self {
            purpose,
            bond: bond.into(),
            vote_period: vote_period.into(),
            grace_period: grace_period.into(),
            policy: vec![PolicyItem {
                max_amount: 0.into(),
                votes: NumOrRatio::Ratio(1, 2),
            }],
            council: UnorderedSet::new(b"c".to_vec()),
            proposals: Vector::new(b"p".to_vec()),
        };
        for account_id in council {
            dao.council.insert(&account_id);
        }
        dao
    }

    #[payable]
    pub fn add_proposal(&mut self, proposal: ProposalInput) -> u64 {
        // TOOD: add also extra storage cost for the proposal itself.
        assert!(env::attached_deposit() >= self.bond, "Not enough deposit");
        assert!(
            proposal.description.len() < MAX_DESCRIPTION_LENGTH,
            "Description length is too long"
        );
        // Input verification.
        match proposal.kind {
            ProposalKind::ChangePolicy { ref policy } => {
                for i in 1..policy.len() {
                    assert!(
                        policy[i].max_amount.0 > policy[i - 1].max_amount.0,
                        "Policy must be sorted, item {} is wrong",
                        i
                    );
                }
            }
            _ => {}
        }
        let p = Proposal {
            status: ProposalStatus::Vote,
            proposer: env::predecessor_account_id(),
            target: proposal.target,
            description: proposal.description,
            kind: proposal.kind,
            vote_period_end: env::block_timestamp() + self.vote_period,
            vote_yes: 0,
            vote_no: 0,
            votes: HashMap::default(),
        };
        self.proposals.push(&p);
        self.proposals.len() - 1
    }

    pub fn get_vote_period(&self) -> WrappedDuration {
        self.vote_period.into()
    }

    pub fn get_bond(&self) -> WrappedBalance {
        self.bond.into()
    }

    pub fn get_council(&self) -> Vec<AccountId> {
        self.council.to_vec()
    }

    pub fn get_num_proposals(&self) -> u64 {
        self.proposals.len()
    }

    pub fn get_proposals(&self, from_index: u64, limit: u64) -> Vec<Proposal> {
        (from_index..std::cmp::min(from_index + limit, self.proposals.len()))
            .map(|index| self.proposals.get(index).unwrap())
            .collect()
    }

    pub fn get_proposal(&self, id: u64) -> Proposal {
        self.proposals.get(id).expect("Proposal not found")
    }

    pub fn get_purpose(&self) -> String {
        self.purpose.clone()
    }

    pub fn vote(&mut self, id: u64, vote: Vote) {
        assert!(
            self.council.contains(&env::predecessor_account_id()),
            "Only council can vote"
        );
        let mut proposal = self.proposals.get(id).expect("No proposal with such id");
        assert_eq!(
            proposal.status,
            ProposalStatus::Vote,
            "Proposal already finalized"
        );
        if proposal.vote_period_end < env::block_timestamp() {
            env::log(b"Voting period expired, finalizing the proposal");
            self.finalize(id);
            return;
        }
        assert!(
            !proposal.votes.contains_key(&env::predecessor_account_id()),
            "Already voted"
        );
        match vote {
            Vote::Yes => proposal.vote_yes += 1,
            Vote::No => proposal.vote_no += 1,
        }
        proposal.votes.insert(env::predecessor_account_id(), vote);
        let post_status = proposal.vote_status(&self.policy, self.council.len());
        // If just changed from vote to Delay, adjust the expiration date to grace period.
        if !post_status.is_finalized() {
            proposal.vote_period_end = env::block_timestamp() + self.grace_period;
            proposal.status = post_status.clone();
        }
        self.proposals.replace(id, &proposal);
        // Finalize if this vote is done.
        if post_status.is_finalized() {
            self.finalize(id);
        }
    }

    pub fn finalize(&mut self, id: u64) {
        let mut proposal = self.proposals.get(id).expect("No proposal with such id");
        assert!(
            !proposal.status.is_finalized(),
            "Proposal already finalized"
        );
        proposal.status = proposal.vote_status(&self.policy, self.council.len());
        match proposal.status {
            ProposalStatus::Success => {
                env::log(b"Vote succeeded");
                let target = proposal.target.clone();
                Promise::new(proposal.proposer.clone()).transfer(self.bond);
                match proposal.kind {
                    ProposalKind::NewCouncil => {
                        self.council.insert(&target);
                    }
                    ProposalKind::RemoveCouncil => {
                        self.council.remove(&target);
                    }
                    ProposalKind::Payout { amount } => {
                        Promise::new(target).transfer(amount.0);
                    }
                    ProposalKind::ChangeVotePeriod { vote_period } => {
                        self.vote_period = vote_period.into();
                    }
                    ProposalKind::ChangeBond { bond } => {
                        self.bond = bond.into();
                    }
                    ProposalKind::ChangePolicy { ref policy } => {
                        self.policy = policy.clone();
                    }
                    ProposalKind::ChangePurpose { ref purpose } => {
                        self.purpose = purpose.clone();
                    }
                };
            }
            ProposalStatus::Reject => {
                env::log(b"Proposal rejected");
            }
            ProposalStatus::Fail => {
                // If no majority vote, let's return the bond.
                env::log(b"Proposal vote failed");
                Promise::new(proposal.proposer.clone()).transfer(self.bond);
            }
            ProposalStatus::Vote | ProposalStatus::Delay => {
                env::panic(b"voting period has not expired and no majority vote yet")
            }
        }
        self.proposals.replace(id, &proposal);
    }
}

#[cfg(test)]
mod tests {
    use near_lib::context::{accounts, VMContextBuilder};
    use near_sdk::{testing_env, MockedBlockchain};

    use super::*;

    fn vote(dao: &mut SputnikDAO, proposal_id: u64, votes: Vec<(usize, Vote)>) {
        for (id, vote) in votes {
            testing_env!(VMContextBuilder::new()
                .predecessor_account_id(accounts(id))
                .finish());
            dao.vote(proposal_id, vote);
        }
    }

    #[test]
    fn test_basics() {
        testing_env!(VMContextBuilder::new().finish());
        let mut dao = SputnikDAO::new(
            "test".to_string(),
            vec![accounts(0), accounts(1)],
            10.into(),
            1_000.into(),
            10.into(),
        );

        assert_eq!(dao.get_bond(), 10.into());
        assert_eq!(dao.get_vote_period(), 1_000.into());
        assert_eq!(dao.get_purpose(), "test");

        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(2))
            .attached_deposit(10)
            .finish());
        let id = dao.add_proposal(ProposalInput {
            target: accounts(2),
            description: "add new member".to_string(),
            kind: ProposalKind::NewCouncil,
        });
        assert_eq!(dao.get_num_proposals(), 1);
        assert_eq!(dao.get_proposals(0, 1).len(), 1);
        vote(&mut dao, id, vec![(0, Vote::Yes)]);
        assert_eq!(dao.get_proposal(id).vote_yes, 1);
        assert_eq!(dao.get_proposal(id).status, ProposalStatus::Vote);
        assert_eq!(dao.get_council(), vec![accounts(0), accounts(1)]);
        vote(&mut dao, id, vec![(1, Vote::Yes)]);
        assert_eq!(
            dao.get_council(),
            vec![accounts(0), accounts(1), accounts(2)]
        );

        // Pay out money for proposal. 2 votes yes vs 1 vote no.
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(2))
            .attached_deposit(10)
            .finish());
        let id = dao.add_proposal(ProposalInput {
            target: accounts(2),
            description: "give me money".to_string(),
            kind: ProposalKind::Payout { amount: 10.into() },
        });
        vote(
            &mut dao,
            id,
            vec![(0, Vote::No), (1, Vote::Yes), (2, Vote::Yes)],
        );
        assert_eq!(dao.get_proposal(id).vote_yes, 2);
        assert_eq!(dao.get_proposal(id).vote_no, 1);
        assert_eq!(dao.get_proposal(id).status, ProposalStatus::Success);

        // No vote for proposal.
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(2))
            .attached_deposit(10)
            .finish());
        let id = dao.add_proposal(ProposalInput {
            target: accounts(2),
            description: "give me more money".to_string(),
            kind: ProposalKind::Payout { amount: 10.into() },
        });
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(3))
            .block_timestamp(1_001)
            .finish());
        dao.finalize(id);
        assert_eq!(dao.get_proposal(id).status, ProposalStatus::Fail);

        // Change policy.
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(2))
            .attached_deposit(10)
            .finish());
        let id = dao.add_proposal(ProposalInput {
            target: accounts(2),
            description: "policy".to_string(),
            kind: ProposalKind::ChangePolicy {
                policy: vec![
                    PolicyItem {
                        max_amount: 100.into(),
                        votes: NumOrRatio::Number(1),
                    },
                    PolicyItem {
                        max_amount: 1_000_000.into(),
                        votes: NumOrRatio::Ratio(1, 1),
                    },
                ],
            },
        });
        vote(&mut dao, id, vec![(0, Vote::Yes), (1, Vote::Yes)]);

        // Try new policy with small amount.
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(2))
            .attached_deposit(10)
            .finish());
        let id = dao.add_proposal(ProposalInput {
            target: accounts(2),
            description: "give me more money".to_string(),
            kind: ProposalKind::Payout { amount: 10.into() },
        });
        vote(&mut dao, id, vec![(0, Vote::Yes)]);
        assert_eq!(dao.get_proposal(id).status, ProposalStatus::Delay);
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(3))
            .block_timestamp(11)
            .finish());
        dao.finalize(id);
        assert_eq!(dao.get_proposal(id).status, ProposalStatus::Success);

        // New policy for bigger amounts requires 100% votes.
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(2))
            .attached_deposit(10)
            .finish());
        let id = dao.add_proposal(ProposalInput {
            target: accounts(2),
            description: "give me more money".to_string(),
            kind: ProposalKind::Payout {
                amount: 10_000.into(),
            },
        });
        vote(&mut dao, id, vec![(0, Vote::Yes)]);
        assert_eq!(dao.get_proposal(id).status, ProposalStatus::Vote);
        vote(&mut dao, id, vec![(1, Vote::Yes)]);
        assert_eq!(dao.get_proposal(id).status, ProposalStatus::Vote);
        vote(&mut dao, id, vec![(2, Vote::Yes)]);
        assert_eq!(dao.get_proposal(id).status, ProposalStatus::Success);
    }

    #[test]
    fn test_single_council() {
        testing_env!(VMContextBuilder::new().finish());
        let mut dao = SputnikDAO::new(
            "".to_string(),
            vec![accounts(0)],
            10.into(),
            1_000.into(),
            10.into(),
        );

        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(2))
            .attached_deposit(10)
            .finish());
        let id = dao.add_proposal(ProposalInput {
            target: accounts(1),
            description: "add new member".to_string(),
            kind: ProposalKind::NewCouncil,
        });
        vote(&mut dao, id, vec![(0, Vote::Yes)]);
        assert_eq!(dao.get_proposal(id).status, ProposalStatus::Success);
        assert_eq!(dao.get_council(), vec![accounts(0), accounts(1)]);
    }

    #[test]
    #[should_panic]
    fn test_double_vote() {
        testing_env!(VMContextBuilder::new().finish());
        let mut dao = SputnikDAO::new(
            "".to_string(),
            vec![accounts(0), accounts(1)],
            10.into(),
            1000.into(),
            10.into(),
        );
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(2))
            .attached_deposit(10)
            .finish());
        let id = dao.add_proposal(ProposalInput {
            target: accounts(2),
            description: "add new member".to_string(),
            kind: ProposalKind::NewCouncil,
        });
        assert_eq!(dao.get_proposals(0, 1).len(), 1);
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(0))
            .finish());
        dao.vote(id, Vote::Yes);
        dao.vote(id, Vote::Yes);
    }

    #[test]
    fn test_two_council() {
        testing_env!(VMContextBuilder::new().finish());
        let mut dao = SputnikDAO::new(
            "".to_string(),
            vec![accounts(0), accounts(1)],
            10.into(),
            1_000.into(),
            10.into(),
        );

        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(2))
            .attached_deposit(10)
            .finish());
        let id = dao.add_proposal(ProposalInput {
            target: accounts(1),
            description: "add new member".to_string(),
            kind: ProposalKind::Payout { amount: 100.into() },
        });
        vote(&mut dao, id, vec![(0, Vote::Yes), (1, Vote::No)]);
        assert_eq!(dao.get_proposal(id).status, ProposalStatus::Fail);
    }

    #[test]
    #[should_panic]
    fn test_run_out_of_money() {
        testing_env!(VMContextBuilder::new().finish());
        let mut dao = SputnikDAO::new(
            "".to_string(),
            vec![accounts(0)],
            10.into(),
            1000.into(),
            10.into(),
        );
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(2))
            .attached_deposit(10)
            .finish());
        let id = dao.add_proposal(ProposalInput {
            target: accounts(2),
            description: "add new member".to_string(),
            kind: ProposalKind::Payout {
                amount: 1000.into(),
            },
        });
        assert_eq!(dao.get_proposals(0, 1).len(), 1);
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(0))
            .account_balance(10)
            .finish());
        dao.vote(id, Vote::Yes);
    }

    #[test]
    #[should_panic]
    fn test_incorrect_policy() {
        testing_env!(VMContextBuilder::new().finish());
        let mut dao = SputnikDAO::new(
            "".to_string(),
            vec![accounts(0), accounts(1)],
            10.into(),
            1000.into(),
            10.into(),
        );
        testing_env!(VMContextBuilder::new()
            .predecessor_account_id(accounts(2))
            .attached_deposit(10)
            .finish());
        dao.add_proposal(ProposalInput {
            target: accounts(2),
            description: "policy".to_string(),
            kind: ProposalKind::ChangePolicy {
                policy: vec![
                    PolicyItem {
                        max_amount: 100.into(),
                        votes: NumOrRatio::Number(5),
                    },
                    PolicyItem {
                        max_amount: 5.into(),
                        votes: NumOrRatio::Number(3),
                    },
                ],
            },
        });
    }
}
