//! Download and resource telemetry events shared by every frontend adapter.

use serde::Serialize;
use specta::Type;
use utoipa::ToSchema;

#[derive(Clone, Debug, Serialize, ToSchema, Type)]
pub struct Download {
    #[schema(value_type = f64)]
    #[specta(type = f64)]
    pub id: u64,
    pub state: DownloadState,
    pub name: Option<String>,
    #[schema(value_type = f64)]
    #[specta(type = f64)]
    pub completed: u64,
    #[schema(value_type = f64)]
    #[specta(type = f64)]
    pub total: u64,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ToSchema, Type)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    Running,
    Finished,
    Failed,
}

#[derive(Clone, Debug, Default, Serialize, ToSchema, Type)]
pub struct ModelResources {
    #[schema(value_type = f64)]
    #[specta(type = f64)]
    pub process_memory: u64,
    #[schema(value_type = f64)]
    #[specta(type = f64)]
    pub system_memory: u64,
    pub process_cpu: f32,
    pub devices: Vec<DeviceResources>,
}

#[derive(Clone, Debug, Default, Serialize, ToSchema, Type)]
pub struct DeviceResources {
    pub name: String,
    pub selected: bool,
    #[schema(value_type = Option<f64>)]
    #[specta(type = Option<f64>)]
    pub memory_budget: Option<u64>,
    #[schema(value_type = Option<f64>)]
    #[specta(type = Option<f64>)]
    pub memory_used: Option<u64>,
    pub utilization: Option<f32>,
}

impl From<koharu_pipeline::ResourceSnapshot> for ModelResources {
    fn from(value: koharu_pipeline::ResourceSnapshot) -> Self {
        Self {
            process_memory: value.process_memory_bytes,
            system_memory: value.system_memory_bytes,
            process_cpu: value.process_cpu_percent,
            devices: value
                .devices
                .into_iter()
                .map(|device| DeviceResources {
                    name: device.name,
                    selected: device.selected,
                    memory_budget: device.memory_budget_bytes,
                    memory_used: device.memory_used_bytes,
                    utilization: device.utilization_percent,
                })
                .collect(),
        }
    }
}
