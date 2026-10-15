//! On-chain client for staked Agent Werewolf games: escrow calls, event reads and receipt signing.

mod client;
mod error;
mod escrow;
mod logs;

pub use client::{ChainClient, ChainConfig, Sent};
pub use error::ChainError;
pub use logs::{LogPosition, LogWatcher};

/// Contract bindings generated from the Solidity sources.
pub mod bindings {
    alloy::sol!(
        #[sol(rpc, all_derives)]
        "../../contracts/src/IWerewolfEscrow.sol"
    );

    alloy::sol!(
        #[sol(all_derives)]
        "../../contracts/src/IWerewolfErrors.sol"
    );

    alloy::sol! {
        /// The ERC20 subset the escrow uses, plus the mock token's `mint`.
        #[sol(rpc, all_derives)]
        interface IERC20 {
            function approve(address spender, uint256 amount) external returns (bool);
            function allowance(address owner, address spender) external view returns (uint256);
            function balanceOf(address account) external view returns (uint256);
            function transfer(address to, uint256 amount) external returns (bool);
            function mint(address to, uint256 amount) external;

            error ERC20InsufficientBalance(address sender, uint256 balance, uint256 needed);
            error ERC20InsufficientAllowance(address spender, uint256 allowance, uint256 needed);
        }
    }

    alloy::sol! {
        /// OpenZeppelin errors the escrow inherits from its libraries.
        #[sol(all_derives)]
        interface OpenZeppelinErrors {
            error ReentrancyGuardReentrantCall();
            error SafeERC20FailedOperation(address token);
        }
    }
}
