// Copyright 2024 The NativeLink Authors. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::serde_utils::{
    convert_data_size_with_shellexpand, convert_duration_with_shellexpand,
    convert_string_with_shellexpand,
};
use crate::stores::StoreRefName;

/// Name of the scheduler. This type will be used when referencing a
/// scheduler in a `topology!` invocation.
pub type SchedulerRefName = String;

/// Used when the config references `instance_name` in the protocol.
pub type InstanceName = String;

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct WithInstanceName<T> {
    #[serde(default)]
    pub instance_name: InstanceName,
    #[serde(flatten)]
    pub config: T,
}

impl<T> core::ops::Deref for WithInstanceName<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct AcStoreConfig {
    /// The store name referenced in the `stores` map in the main config.
    /// This store name referenced here may be reused multiple times.
    #[serde(deserialize_with = "convert_string_with_shellexpand")]
    pub ac_store: StoreRefName,

    /// Whether the Action Cache store may be written to, this if set to false
    /// it is only possible to read from the Action Cache.
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct CasStoreConfig {
    /// The store name referenced in the `stores` map in the main config.
    /// This store name referenced here may be reused multiple times.
    #[serde(deserialize_with = "convert_string_with_shellexpand")]
    pub cas_store: StoreRefName,
}

#[derive(Deserialize, Serialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesRemoteExecutionConfig {
    /// Scheduler used to configure the capabilities of remote execution.
    #[serde(deserialize_with = "convert_string_with_shellexpand")]
    pub scheduler: SchedulerRefName,
}

#[derive(Deserialize, Serialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesConfig {
    /// Configuration for remote execution capabilities.
    /// If not set the capabilities service will inform the client that remote
    /// execution is not supported.
    pub remote_execution: Option<CapabilitiesRemoteExecutionConfig>,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    /// The store name referenced in the `stores` map in the main config.
    /// This store name referenced here may be reused multiple times.
    /// This value must be a CAS store reference.
    #[serde(deserialize_with = "convert_string_with_shellexpand")]
    pub cas_store: StoreRefName,

    /// The scheduler name referenced in the `schedulers` map in the main config.
    #[serde(deserialize_with = "convert_string_with_shellexpand")]
    pub scheduler: SchedulerRefName,
}

#[derive(Deserialize, Serialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct ByteStreamConfig {
    /// Name of the store in the "stores" configuration.
    pub cas_stores: HashMap<InstanceName, StoreRefName>,

    /// Max number of bytes to send on each grpc stream chunk.
    /// According to <https://github.com/grpc/grpc.github.io/issues/371>
    /// 16KiB - 64KiB is optimal.
    ///
    ///
    /// Default: 64KiB
    #[serde(default, deserialize_with = "convert_data_size_with_shellexpand")]
    pub max_bytes_per_stream: usize,

    /// Maximum number of bytes to decode on each grpc stream chunk.
    /// Default: 4 MiB
    #[serde(default, deserialize_with = "convert_data_size_with_shellexpand")]
    pub max_decoding_message_size: usize,

    /// In the event a client disconnects while uploading a blob, we will hold
    /// the internal stream open for this many seconds before closing it.
    /// This allows clients that disconnect to reconnect and continue uploading
    /// the same blob.
    ///
    /// Default: 10 (seconds)
    #[serde(default, deserialize_with = "convert_duration_with_shellexpand")]
    pub persist_stream_on_disconnect_timeout: usize,
}
