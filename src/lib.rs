mod error;
mod gpu;
mod sources;

use error::*;
use ff::Field;
pub use gpu::*;
use paired::bls12_381::Fr;

// TODO: Move these constants into configuration of GPU, Sealer, KeyGenerator, etc.
const COMBINE_BATCH_SIZE: usize = 500000;

#[derive(PartialEq, Debug, Clone, Copy)]
#[repr(transparent)]
pub struct Node(pub Fr);

impl Default for Node {
    fn default() -> Self {
        Node(Fr::zero())
    }
}

#[derive(PartialEq, Debug, Clone, Copy)]
pub struct Sha256Domain(pub [u8; 32]);

impl Default for Sha256Domain {
    fn default() -> Self {
        Self([0u8; 32])
    }
}

pub struct Layer(pub Vec<Node>);

pub trait NarrowStackedExpander: Sized {
    fn new(config: Config) -> NSEResult<Self>;
    fn generate_mask_layer(
        &mut self,
        replica_id: Sha256Domain,
        window_index: usize,
    ) -> NSEResult<Layer>;
    fn generate_expander_layer(
        &mut self,
        replica_id: Sha256Domain,
        window_index: usize,
        layer_index: usize,
    ) -> NSEResult<Layer>;
    fn generate_butterfly_layer(
        &mut self,
        replica_id: Sha256Domain,
        window_index: usize,
        layer_index: usize,
    ) -> NSEResult<Layer>;
    // Combine functions need to get `&mut self`, as they modify internal state of GPU buffers
    fn combine_layer(&mut self, layer: &Layer, is_decode: bool) -> NSEResult<Layer> {
        Ok(Layer(self.combine_segment(0, &layer.0, is_decode)?))
    }
    fn combine_segment(
        &mut self,
        offset: usize,
        segment: &[Node],
        is_decode: bool,
    ) -> NSEResult<Vec<Node>>;
    fn combine_batch_size(&self) -> usize;
    fn leaf_count(&self) -> usize;
}

// NOTES:
// layers are 1-indexed,

/// The configuration parameters for NSE.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Batch hashing factor.
    pub k: u32,
    /// Number of nodes per window
    pub num_nodes_window: usize,
    /// Degree of the expander graph.
    pub degree_expander: usize,
    /// Degree of the butterfly graph.
    pub degree_butterfly: usize,
    /// Number of expander layers.
    pub num_expander_layers: usize, // 8
    /// Number of butterfly layers.
    pub num_butterfly_layers: usize, // 7
}

pub struct Sealer {
    original_data: Layer,
    key_generator: KeyGenerator,
}

impl Sealer {
    pub fn new(
        config: Config,
        replica_id: Sha256Domain,
        window_index: usize,
        original_data: Layer,
        gpu: GPU,
    ) -> NSEResult<Self> {
        Ok(Self {
            original_data,
            key_generator: KeyGenerator::new(config, replica_id, window_index, gpu)?,
        })
    }
}

impl Iterator for Sealer {
    type Item = Layer;

    /// Returns successive layers, starting with mask layer, and ending with sealed replica layer.
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(next_key_layer) = self.key_generator.next() {
            if self.key_generator.layers_remaining() == 0 {
                Some(
                    // TODO: Remove `unwrap()`, handle errors
                    self.key_generator
                        .combine_layer(&self.original_data, false)
                        .unwrap(),
                )
            } else {
                Some(next_key_layer)
            }
        } else {
            None
        }
    }
}

impl ExactSizeIterator for Sealer {
    fn len(&self) -> usize {
        self.key_generator.len()
    }
}

pub struct Unsealer {
    sealed_data: Layer,
    key_generator: KeyGenerator,
}

impl Unsealer {
    pub fn new(
        config: Config,
        replica_id: Sha256Domain,
        window_index: usize,
        sealed_data: Layer,
        gpu: GPU,
    ) -> NSEResult<Self> {
        Ok(Self {
            sealed_data,
            key_generator: KeyGenerator::new(config, replica_id, window_index, gpu)?,
        })
    }
}

impl Iterator for Unsealer {
    type Item = Layer;

    /// Returns successive layers, starting with mask layer, and ending with sealed replica layer.
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(next_key_layer) = self.key_generator.next() {
            if self.key_generator.layers_remaining() == 0 {
                Some(
                    // TODO: Remove `unwrap()`, handle errors
                    self.key_generator
                        .combine_layer(&self.sealed_data, true)
                        .unwrap(),
                )
            } else {
                Some(next_key_layer)
            }
        } else {
            None
        }
    }
}

impl ExactSizeIterator for Unsealer {
    fn len(&self) -> usize {
        self.key_generator.len()
    }
}

pub struct KeyGenerator {
    replica_id: Sha256Domain,
    window_index: usize,
    current_layer_index: usize,
    gpu: GPU,
}

impl KeyGenerator {
    fn new(
        config: Config,
        replica_id: Sha256Domain,
        window_index: usize,
        gpu: GPU,
    ) -> NSEResult<Self> {
        assert_eq!(config.num_nodes_window, gpu.leaf_count());
        Ok(Self {
            replica_id,
            window_index,
            current_layer_index: 0, // Initial value of 0 means the current layer precedes any generated layer.
            gpu,
        })
    }

    fn config(&self) -> Config {
        self.gpu.config
    }

    fn layers_remaining(&self) -> usize {
        self.len() - self.current_layer_index
    }

    // Generate maske layer on GPU from seeds.
    fn generate_mask_layer(&mut self) -> NSEResult<Layer> {
        self.gpu
            .generate_mask_layer(self.replica_id, self.window_index)
    }

    // Generate expander layer on GPU, using previous layer already loaded.
    fn generate_expander_layer(&mut self) -> NSEResult<Layer> {
        self.gpu.generate_expander_layer(
            self.replica_id,
            self.window_index,
            self.current_layer_index,
        )
    }
    // Generate butterfly layer on GPU, using previous layer already loaded.
    fn generate_butterfly_layer(&mut self) -> NSEResult<Layer> {
        self.gpu.generate_expander_layer(
            self.replica_id,
            self.window_index,
            self.current_layer_index,
        )
    }

    fn combine_layer(&mut self, layer: &Layer, is_decode: bool) -> NSEResult<Layer> {
        self.gpu.combine_layer(layer, is_decode)
    }
}

impl Iterator for KeyGenerator {
    type Item = Layer;

    fn next(&mut self) -> Option<Self::Item> {
        let last_index = self.config().num_expander_layers + self.config().num_butterfly_layers;

        // If current index is last, then we have already finished generating layers.
        if self.current_layer_index >= last_index {
            return None;
        }
        self.current_layer_index += 1;

        // First layer is mask layer.
        if self.current_layer_index == 1 {
            // TODO: Remove `unwrap()`, handle errors
            return Some(self.generate_mask_layer().unwrap());
        }

        // When current index equals number of expander layers, we need to generate the last expander layer.
        // Before that, generate earlier expander layers.
        if self.current_layer_index <= self.config().num_expander_layers {
            // TODO: Remove `unwrap()`, handle errors
            return Some(self.generate_expander_layer().unwrap());
        }

        // When current index equals last index (having been incremented since the first check),
        // we need to generate the last butterfly layer. Before that, generate earlier butterfly layers.
        if self.current_layer_index <= last_index {
            // TODO: Remove `unwrap()`, handle errors
            return Some(self.generate_butterfly_layer().unwrap());
        };

        unreachable!();
    }
}

impl ExactSizeIterator for KeyGenerator {
    fn len(&self) -> usize {
        self.config().num_expander_layers + self.config().num_butterfly_layers
    }
}
