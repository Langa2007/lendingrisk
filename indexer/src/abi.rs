//! Solidity ABI definitions (events we decode, and the view calls we make).
use alloy::sol;

sol! {
    // ---- Aave v3 Pool events ----
    event Supply(address indexed reserve, address user, address indexed onBehalfOf, uint256 amount, uint16 indexed referralCode);
    event Withdraw(address indexed reserve, address indexed user, address indexed to, uint256 amount);
    event Borrow(address indexed reserve, address user, address indexed onBehalfOf, uint256 amount, uint8 interestRateMode, uint256 borrowRate, uint16 indexed referralCode);
    event Repay(address indexed reserve, address indexed user, address indexed repayer, uint256 amount, bool useATokens);
    event LiquidationCall(address indexed collateralAsset, address indexed debtAsset, address indexed user, uint256 debtToCover, uint256 liquidatedCollateralAmount, address liquidator, bool receiveAToken);
    event ReserveUsedAsCollateralEnabled(address indexed reserve, address indexed user);
    event ReserveUsedAsCollateralDisabled(address indexed reserve, address indexed user);
    event ReserveDataUpdated(address indexed reserve, uint256 liquidityRate, uint256 stableBorrowRate, uint256 variableBorrowRate, uint256 liquidityIndex, uint256 variableBorrowIndex);
    event UserEModeSet(address indexed user, uint8 categoryId);

    // ---- aToken event: balances move without Pool events ----
    event BalanceTransfer(address indexed from, address indexed to, uint256 value, uint256 index);
}

sol! {
    // ---- view calls ----
    function getPool() external view returns (address);
    function getPoolDataProvider() external view returns (address);
    function getReservesList() external view returns (address[] memory);
    function getReserveTokensAddresses(address asset) external view returns (address aTokenAddress, address stableDebtTokenAddress, address variableDebtTokenAddress);
    function getUserReserveData(address asset, address user) external view returns (
        uint256 currentATokenBalance, uint256 currentStableDebt, uint256 currentVariableDebt,
        uint256 principalStableDebt, uint256 scaledVariableDebt, uint256 stableBorrowRate,
        uint256 liquidityRate, uint40 stableRateLastUpdated, bool usageAsCollateralEnabled);
    function scaledBalanceOf(address user) external view returns (uint256);
    function symbol() external view returns (string memory);
    function decimals() external view returns (uint8);

    // ---- Multicall3 ----
    struct Call3 { address target; bool allowFailure; bytes callData; }
    struct Call3Result { bool success; bytes returnData; }
    function aggregate3(Call3[] calldata calls) external payable returns (Call3Result[] memory returnData);
}

/// Multicall3 is deployed at the same address on every major chain.
pub const MULTICALL3: alloy::primitives::Address =
    alloy::primitives::address!("cA11bde05977b3631167028862bE2a173976CA11");
