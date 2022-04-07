use rand::Rng;

use casper_engine_test_support::{
    DeployItemBuilder, ExecuteRequestBuilder, InMemoryWasmTestBuilder, DEFAULT_ACCOUNTS,
    DEFAULT_ACCOUNT_ADDR,
};
use casper_execution_engine::{
    core::engine_state::ExecuteRequest, storage::transaction_source::in_memory::InMemoryEnvironment,
};
use casper_types::{
    account::AccountHash, bytesrepr::FromBytes, runtime_args, system::mint, CLTyped, Contract,
    ContractHash, ContractPackageHash, Key, PublicKey, RuntimeArgs, URef, U512,
};

use super::{
    ARG_AMOUNT, ARG_AVAILABLE_AMOUNT, ARG_DISTRIBUTIONS_PER_INTERVAL, ARG_ID, ARG_TARGET,
    ARG_TIME_INTERVAL, ENTRY_POINT_AUTHORIZE_TO, ENTRY_POINT_FAUCET, ENTRY_POINT_SET_VARIABLES,
    FAUCET_CALL_DEFAULT_PAYMENT, FAUCET_CONTRACT_NAMED_KEY, FAUCET_FUND_AMOUNT, FAUCET_ID,
    FAUCET_INSTALLER_SESSION, FAUCET_PURSE_NAMED_KEY, INSTALLER_ACCOUNT, INSTALLER_FUND_AMOUNT,
    REMAINING_REQUESTS_NAMED_KEY, TWO_HOURS_AS_MILLIS,
};

#[derive(Clone, Copy, Debug)]
pub struct FundAccountRequestBuilder {
    target_account: AccountHash,
    fund_amount: U512,
    fund_id: Option<u64>,
}

impl FundAccountRequestBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_target_account(mut self, account_hash: AccountHash) -> Self {
        self.target_account = account_hash;
        self
    }

    pub fn with_fund_amount(mut self, fund_amount: U512) -> Self {
        self.fund_amount = fund_amount;
        self
    }

    pub fn with_fund_id(mut self, fund_id: Option<u64>) -> Self {
        self.fund_id = fund_id;
        self
    }

    pub fn build(&self) -> ExecuteRequest {
        ExecuteRequestBuilder::transfer(
            *DEFAULT_ACCOUNT_ADDR,
            runtime_args! {
                mint::ARG_TARGET => self.target_account,
                mint::ARG_AMOUNT => self.fund_amount,
                mint::ARG_ID => self.fund_id
            },
        )
        .build()
    }
}

impl Default for FundAccountRequestBuilder {
    fn default() -> Self {
        Self {
            target_account: *INSTALLER_ACCOUNT,
            fund_amount: U512::from(INSTALLER_FUND_AMOUNT),
            fund_id: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FaucetInstallSessionRequestBuilder {
    installer_account: AccountHash,
    faucet_installer_session: String,
    faucet_id: u64,
    faucet_fund_amount: U512,
}

impl FaucetInstallSessionRequestBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_installer_account(mut self, installer_account: AccountHash) -> Self {
        self.installer_account = installer_account;
        self
    }

    pub fn with_faucet_installer_session(mut self, installer_session: &str) -> Self {
        self.faucet_installer_session = installer_session.to_string();
        self
    }

    pub fn with_faucet_id(mut self, faucet_id: u64) -> Self {
        self.faucet_id = faucet_id;
        self
    }

    pub fn with_faucet_fund_amount(mut self, faucet_fund_amount: U512) -> Self {
        self.faucet_fund_amount = faucet_fund_amount;
        self
    }

    pub fn build(&self) -> ExecuteRequest {
        ExecuteRequestBuilder::standard(
            self.installer_account,
            &self.faucet_installer_session,
            runtime_args! {
                ARG_ID => self.faucet_id,
                ARG_AMOUNT => self.faucet_fund_amount
            },
        )
        .build()
    }
}

impl Default for FaucetInstallSessionRequestBuilder {
    fn default() -> Self {
        Self {
            installer_account: *INSTALLER_ACCOUNT,
            faucet_installer_session: FAUCET_INSTALLER_SESSION.to_string(),
            faucet_id: FAUCET_ID,
            faucet_fund_amount: FAUCET_FUND_AMOUNT.into(),
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct FaucetConfigRequestBuilder {
    installer_account: AccountHash,
    faucet_contract_hash: Option<ContractHash>,
    available_amount: Option<U512>,
    time_interval: Option<u64>,
    distributions_per_interval: Option<u64>,
}

impl FaucetConfigRequestBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_installer_account(mut self, installer_account: AccountHash) -> Self {
        self.installer_account = installer_account;
        self
    }

    pub fn with_faucet_contract_hash(mut self, contract_hash: ContractHash) -> Self {
        self.faucet_contract_hash = Some(contract_hash);
        self
    }

    pub fn with_available_amount(mut self, available_amount: Option<U512>) -> Self {
        self.available_amount = available_amount;
        self
    }

    pub fn with_time_interval(mut self, time_interval: Option<u64>) -> Self {
        self.time_interval = time_interval;
        self
    }

    pub fn with_distributions_per_interval(
        mut self,
        distributions_per_interval: Option<u64>,
    ) -> Self {
        self.distributions_per_interval = distributions_per_interval;
        self
    }

    pub fn build(&self) -> ExecuteRequest {
        ExecuteRequestBuilder::contract_call_by_hash(
            self.installer_account,
            self.faucet_contract_hash
                .expect("must supply faucet contract hash"),
            ENTRY_POINT_SET_VARIABLES,
            runtime_args! {
                ARG_AVAILABLE_AMOUNT => self.available_amount,
                ARG_TIME_INTERVAL => self.time_interval,
                ARG_DISTRIBUTIONS_PER_INTERVAL => self.distributions_per_interval
            },
        )
        .build()
    }
}

impl Default for FaucetConfigRequestBuilder {
    fn default() -> Self {
        Self {
            installer_account: *INSTALLER_ACCOUNT,
            faucet_contract_hash: None,
            available_amount: None,
            time_interval: None,
            distributions_per_interval: None,
        }
    }
}

pub struct FaucetAuthorizeAccountRequestBuilder {
    installer_account: AccountHash,
    authorized_account_public_key: Option<PublicKey>,
    faucet_contract_hash: Option<ContractHash>,
}

impl FaucetAuthorizeAccountRequestBuilder {
    pub fn with_installer_account(mut self, installer_account: AccountHash) -> Self {
        self.installer_account = installer_account;
        self
    }

    pub fn with_authorized_user_public_key(
        mut self,
        authorized_account_public_key: Option<PublicKey>,
    ) -> Self {
        self.authorized_account_public_key = authorized_account_public_key;
        self
    }

    pub fn build(self) -> ExecuteRequest {
        ExecuteRequestBuilder::contract_call_by_hash(
            self.installer_account,
            self.faucet_contract_hash
                .expect("must supply faucet contract hash"),
            ENTRY_POINT_AUTHORIZE_TO,
            runtime_args! {ARG_TARGET => self.authorized_account_public_key},
        )
        .build()
    }
}

impl Default for FaucetAuthorizeAccountRequestBuilder {
    fn default() -> Self {
        Self {
            installer_account: *INSTALLER_ACCOUNT,
            authorized_account_public_key: None,
            faucet_contract_hash: None,
        }
    }
}

enum FaucetCallerAccount {
    Installer(AccountHash),
    Authorized(AccountHash),
    User(AccountHash),
}

impl FaucetCallerAccount {
    pub fn account_hash(&self) -> AccountHash {
        match self {
            FaucetCallerAccount::Installer(account_hash)
            | FaucetCallerAccount::Authorized(account_hash)
            | FaucetCallerAccount::User(account_hash) => account_hash.clone(),
        }
    }
}

pub struct FaucetFundRequestBuilder {
    faucet_contract_hash: Option<ContractHash>,
    caller_account: FaucetCallerAccount,
    arg_target: Option<AccountHash>,
    arg_fund_amount: Option<U512>,
    arg_id: Option<u64>,
    payment_amount: U512,
}

impl FaucetFundRequestBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_installer_account(mut self, installer_account: AccountHash) -> Self {
        self.caller_account = FaucetCallerAccount::Installer(installer_account);
        self
    }

    pub fn with_authorized_account(mut self, authorized_account: AccountHash) -> Self {
        self.caller_account = FaucetCallerAccount::Authorized(authorized_account);
        self
    }

    pub fn with_user_account(mut self, user_account: AccountHash) -> Self {
        self.caller_account = FaucetCallerAccount::User(user_account);
        self
    }

    pub fn with_arg_fund_amount(mut self, fund_amount: U512) -> Self {
        self.arg_fund_amount = Some(fund_amount);
        self
    }

    pub fn with_arg_target(mut self, target: AccountHash) -> Self {
        self.arg_target = Some(target);
        self
    }

    pub fn with_arg_id(mut self, id: Option<u64>) -> Self {
        self.arg_id = id;
        self
    }

    pub fn with_payment_amount(mut self, payment_amount: U512) -> Self {
        self.payment_amount = payment_amount;
        self
    }

    pub fn with_faucet_contract_hash(mut self, faucet_contract_hash: ContractHash) -> Self {
        self.faucet_contract_hash = Some(faucet_contract_hash);
        self
    }

    pub fn build(self) -> ExecuteRequest {
        let mut rng = rand::thread_rng();

        let deploy_item = DeployItemBuilder::new()
            .with_address(self.caller_account.account_hash())
            .with_authorization_keys(&[self.caller_account.account_hash()])
            .with_stored_session_hash(
                self.faucet_contract_hash
                    .expect("must supply faucet contract hash"),
                ENTRY_POINT_FAUCET,
                match self.caller_account {
                    FaucetCallerAccount::Installer(_)
                    | FaucetCallerAccount::Authorized(_) => runtime_args! {
                        ARG_TARGET => self.arg_target.expect("must supply arg target when calling as installer or authorized account"),
                        ARG_AMOUNT => self.arg_fund_amount.expect("must supply arg amount when calling as installer or authorized account"),
                        ARG_ID => self.arg_id
                    },
                    FaucetCallerAccount::User(_) => runtime_args! {
                       ARG_ID => self.arg_id
                    },
                },
            )
            .with_empty_payment_bytes(runtime_args! {ARG_AMOUNT => self.payment_amount})
            .with_deploy_hash(rng.gen())
            .build();

        ExecuteRequestBuilder::from_deploy_item(deploy_item).build()
    }
}

impl Default for FaucetFundRequestBuilder {
    fn default() -> Self {
        Self {
            arg_fund_amount: None,
            payment_amount: U512::from(FAUCET_CALL_DEFAULT_PAYMENT),
            faucet_contract_hash: None,
            caller_account: FaucetCallerAccount::Installer(*INSTALLER_ACCOUNT),
            arg_target: None,
            arg_id: None,
        }
    }
}

pub fn query_stored_value<T: CLTyped + FromBytes>(
    builder: &mut InMemoryWasmTestBuilder,
    base_key: Key,
    path: Vec<String>,
) -> T {
    builder
        .query(None, base_key, &path)
        .expect("must have stored value")
        .as_cl_value()
        .cloned()
        .expect("must have cl value")
        .into_t::<T>()
        .expect("must get value")
}

pub fn get_faucet_contract_hash(
    builder: &InMemoryWasmTestBuilder,
    installer_account: AccountHash,
) -> ContractHash {
    builder
        .get_expected_account(installer_account)
        .named_keys()
        .get(&format!("{}_{}", FAUCET_CONTRACT_NAMED_KEY, FAUCET_ID))
        .cloned()
        .and_then(Key::into_hash)
        .map(ContractHash::new)
        .expect("failed to find faucet contract")
}

pub fn get_faucet_key(builder: &InMemoryWasmTestBuilder, installer_account: AccountHash) -> Key {
    builder
        .get_expected_account(installer_account)
        .named_keys()
        .get(&format!("{}_{}", FAUCET_CONTRACT_NAMED_KEY, FAUCET_ID))
        .cloned()
        .expect("failed to find faucet key")
}

pub fn get_faucet_contract(
    builder: &InMemoryWasmTestBuilder,
    installer_account: AccountHash,
) -> Contract {
    builder
        .get_contract(get_faucet_contract_hash(builder, installer_account))
        .expect("failed to find faucet contract")
}

pub fn get_faucet_purse(builder: &InMemoryWasmTestBuilder, installer_account: AccountHash) -> URef {
    get_faucet_contract(builder, installer_account)
        .named_keys()
        .get(FAUCET_PURSE_NAMED_KEY)
        .cloned()
        .and_then(Key::into_uref)
        .expect("failed to find faucet purse")
}

pub struct FaucetDeployHelper {
    installer_account: AccountHash,
    installer_fund_amount: U512,
    installer_fund_id: Option<u64>,
    authorized_account: Option<AccountHash>,
    faucet_purse_fund_amount: U512,
    faucet_installer_session: String,
    faucet_id: u64,
    faucet_contract_hash: Option<ContractHash>,
    faucet_distributions_per_interval: Option<u64>,
    faucet_available_amount: Option<U512>,
    faucet_time_interval: Option<u64>,
    fund_account_request_builder: FundAccountRequestBuilder,
    pub faucet_install_session_request_builder: FaucetInstallSessionRequestBuilder,
    pub faucet_config_request_builder: FaucetConfigRequestBuilder,
    pub faucet_authorize_account_request_builder: FaucetAuthorizeAccountRequestBuilder,
    pub faucet_fund_request_builder: FaucetFundRequestBuilder,
}

impl FaucetDeployHelper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn installer_account(&self) -> AccountHash {
        self.installer_account.clone()
    }

    pub fn with_installer_account(mut self, installer_account: AccountHash) -> Self {
        self.installer_account = installer_account;
        self
    }

    pub fn with_installer_fund_amount(mut self, installer_fund_amount: U512) -> Self {
        self.installer_fund_amount = installer_fund_amount;
        self
    }

    pub fn with_installer_fund_id(mut self, installer_fund_id: Option<u64>) -> Self {
        self.installer_fund_id = installer_fund_id;
        self
    }

    pub fn with_faucet_id(mut self, faucet_id: u64) -> Self {
        self.faucet_id = faucet_id;
        self
    }

    pub fn with_faucet_purse_fund_amount(mut self, faucet_purse_fund_amount: U512) -> Self {
        self.faucet_purse_fund_amount = faucet_purse_fund_amount;
        self
    }

    pub fn with_faucet_available_amount(mut self, available_amount: Option<U512>) -> Self {
        self.faucet_available_amount = available_amount;
        self
    }

    pub fn with_faucet_distributions_per_interval(
        mut self,
        distributions_per_interval: Option<u64>,
    ) -> Self {
        self.faucet_distributions_per_interval = distributions_per_interval;
        self
    }

    pub fn with_faucet_time_interval(mut self, time_interval_ms: Option<u64>) -> Self {
        self.faucet_time_interval = time_interval_ms;
        self
    }

    pub fn query_and_set_faucet_contract_hash(
        &mut self,
        builder: &InMemoryWasmTestBuilder,
    ) -> ContractHash {
        let contract_hash = get_faucet_contract_hash(builder, self.installer_account());
        self.faucet_contract_hash = Some(contract_hash);

        contract_hash
    }

    pub fn query_faucet_purse(&self, builder: &InMemoryWasmTestBuilder) -> URef {
        get_faucet_purse(builder, self.installer_account())
    }

    pub fn query_faucet_purse_balance(&self, builder: &InMemoryWasmTestBuilder) -> U512 {
        let faucet_purse = self.query_faucet_purse(&builder);
        builder.get_purse_balance(faucet_purse)
    }

    pub fn query_remaining_requests(&self, builder: &mut InMemoryWasmTestBuilder) -> U512 {
        query_stored_value(
            builder,
            self.faucet_contract_hash
                .expect("faucet contract hash must be set")
                .into(),
            vec![REMAINING_REQUESTS_NAMED_KEY.to_string()],
        )
    }

    pub fn faucet_purse_fund_amount(&self) -> U512 {
        self.faucet_purse_fund_amount
    }

    pub fn faucet_contract_hash(&self) -> Option<ContractHash> {
        self.faucet_contract_hash.clone()
    }

    pub fn faucet_distributions_per_interval(&self) -> Option<u64> {
        self.faucet_distributions_per_interval
    }

    pub fn faucet_available_amount(&self) -> Option<U512> {
        self.faucet_available_amount
    }

    pub fn faucet_time_interval(&self) -> Option<u64> {
        self.faucet_time_interval
    }

    pub fn fund_installer_request(&self) -> ExecuteRequest {
        self.fund_account_request_builder
            .with_target_account(self.installer_account)
            .with_fund_amount(self.installer_fund_amount)
            .with_fund_id(self.installer_fund_id)
            .build()
    }

    pub fn faucet_install_request(&self) -> ExecuteRequest {
        self.faucet_install_session_request_builder
            .clone()
            .with_installer_account(self.installer_account)
            .with_faucet_id(self.faucet_id)
            .with_faucet_fund_amount(self.faucet_purse_fund_amount)
            .with_faucet_installer_session(&self.faucet_installer_session)
            .build()
    }

    pub fn faucet_config_request(&self) -> ExecuteRequest {
        self.faucet_config_request_builder
            .with_installer_account(self.installer_account())
            .with_faucet_contract_hash(
                self.faucet_contract_hash()
                    .expect("must supply faucet contract hash"),
            )
            .with_distributions_per_interval(self.faucet_distributions_per_interval)
            .with_available_amount(self.faucet_available_amount)
            .with_time_interval(self.faucet_time_interval)
            .build()
    }

    pub fn new_faucet_fund_request_builder(&self) -> FaucetFundRequestBuilder {
        FaucetFundRequestBuilder::new().with_faucet_contract_hash(
            self.faucet_contract_hash()
                .expect("must supply faucet contract hash"),
        )
    }
}

impl Default for FaucetDeployHelper {
    fn default() -> Self {
        Self {
            installer_fund_amount: U512::from(INSTALLER_FUND_AMOUNT),
            installer_account: *INSTALLER_ACCOUNT,
            installer_fund_id: None,
            authorized_account: None,
            faucet_installer_session: FAUCET_INSTALLER_SESSION.to_string(),
            faucet_id: FAUCET_ID,
            faucet_purse_fund_amount: U512::from(FAUCET_FUND_AMOUNT),
            faucet_contract_hash: None,
            faucet_distributions_per_interval: None,
            faucet_available_amount: None,
            faucet_time_interval: None,
            fund_account_request_builder: Default::default(),
            faucet_install_session_request_builder: Default::default(),
            faucet_config_request_builder: Default::default(),
            faucet_authorize_account_request_builder: Default::default(),
            faucet_fund_request_builder: Default::default(),
        }
    }
}
