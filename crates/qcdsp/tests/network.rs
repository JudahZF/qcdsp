//! IR 3 networks against the scalar graph: the same NAM-style WaveNet built both ways must give
//! the same audio on the host. The scalar form is the one that matched NAM core and played on a
//! unit, so this pins the network's convolution order, history and activation.

use qcdsp::{
    Activation, Block, BlockBuilder, Category, NetworkBuilder, Parameter, Processor, Signal,
    network::Buffer,
};

/// One layer array: input size, channels, head size, head bias, kernel, dilations.
struct Array {
    input: usize,
    channels: usize,
    head: usize,
    head_bias: bool,
    kernel: usize,
    dilations: &'static [usize],
}

const ARRAYS: [Array; 2] = [
    Array {
        input: 1,
        channels: 4,
        head: 2,
        head_bias: false,
        kernel: 3,
        dilations: &[1, 2, 4, 8],
    },
    Array {
        input: 4,
        channels: 2,
        head: 1,
        head_bias: true,
        kernel: 3,
        dilations: &[1, 2, 4],
    },
];

/// The weights in NAM's file order.
struct Weights {
    values: Vec<f32>,
    at: usize,
}

impl Weights {
    fn random() -> Self {
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let values = (0..20_000)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed >> 40) as f32 / (1u64 << 24) as f32 - 0.5
            })
            .collect();
        Self { values, at: 0 }
    }
    fn take(&mut self, n: usize) -> Vec<f32> {
        self.at += n;
        self.values[self.at - n..self.at].to_vec()
    }
}

fn parameters(dsp: &mut BlockBuilder) -> (Signal, Signal) {
    let input = dsp
        .parameter(Parameter::new("Input", 0.0, 4.0, 1.0).unwrap())
        .unwrap();
    let output = dsp
        .parameter(Parameter::new("Output", 0.0, 4.0, 1.0).unwrap())
        .unwrap();
    (input, output)
}

/// `bias + Σ weights[row·n + j] · values[j]` per row, in the order of NAM's extension.
fn matrix(
    dsp: &mut BlockBuilder,
    w: &[f32],
    values: &[Signal],
    bias: Option<&[f32]>,
) -> Vec<Signal> {
    let n = values.len();
    (0..w.len() / n)
        .map(|row| {
            let mut sum = dsp.constant(bias.map_or(0.0, |b| b[row])).unwrap();
            for (j, &value) in values.iter().enumerate() {
                let weight = dsp.constant(w[row * n + j]).unwrap();
                let product = dsp.mul(weight, value).unwrap();
                sum = dsp.add(sum, product).unwrap();
            }
            sum
        })
        .collect()
}

/// The scalar graph, as the NAM extension builds it today (delay lines for the taps).
fn scalar() -> Block {
    let mut w = Weights::random();
    let mut dsp = BlockBuilder::new("wn_scalar", "WaveNet scalar", Category::GuitarAmplifier);
    dsp.mono();
    let (input_gain, output_gain) = parameters(&mut dsp);
    let x = dsp.mul(dsp.input(), input_gain).unwrap();
    let mut input = vec![x];
    let mut head: Option<Vec<Signal>> = None;
    for array in &ARRAYS {
        let c = array.channels;
        let k = array.kernel;
        let mut v = matrix(&mut dsp, &w.take(c * array.input), &input, None);
        let mut sum = head.clone();
        for &dilation in array.dilations {
            let conv = w.take(c * c * k);
            let conv_bias = w.take(c);
            let mixin = w.take(c);
            let layer = w.take(c * c);
            let layer_bias = w.take(c);
            // past[m][j]: v[j] from m·dilation samples ago.
            let mut past = vec![v.clone()];
            for m in 1..k {
                let previous = past[m - 1].clone();
                past.push(
                    previous
                        .into_iter()
                        .map(|value| {
                            let line = dsp.delay(dilation).unwrap();
                            dsp.update_delay(line, value).unwrap();
                            line.signal()
                        })
                        .collect(),
                );
            }
            let a: Vec<Signal> = (0..c)
                .map(|o| {
                    let mut pre = dsp.constant(conv_bias[o]).unwrap();
                    for j in 0..c {
                        for tap in 0..k {
                            let weight = dsp.constant(conv[(o * c + j) * k + tap]).unwrap();
                            let product = dsp.mul(weight, past[k - 1 - tap][j]).unwrap();
                            pre = dsp.add(pre, product).unwrap();
                        }
                    }
                    let weight = dsp.constant(mixin[o]).unwrap();
                    let product = dsp.mul(weight, x).unwrap();
                    pre = dsp.add(pre, product).unwrap();
                    dsp.tanh(pre).unwrap()
                })
                .collect();
            sum = Some(match sum {
                None => a.clone(),
                Some(sum) => sum
                    .iter()
                    .zip(&a)
                    .map(|(&s, &value)| dsp.add(s, value).unwrap())
                    .collect(),
            });
            let mixed = matrix(&mut dsp, &layer, &a, Some(&layer_bias));
            v = v
                .iter()
                .zip(&mixed)
                .map(|(&value, &m)| dsp.add(value, m).unwrap())
                .collect();
        }
        let head_weights = w.take(array.head * c);
        let head_bias = array.head_bias.then(|| w.take(array.head));
        head = Some(matrix(
            &mut dsp,
            &head_weights,
            sum.as_ref().unwrap(),
            head_bias.as_deref(),
        ));
        input = v;
    }
    let scale = dsp.constant(w.take(1)[0]).unwrap();
    let scaled = dsp.mul(head.unwrap()[0], scale).unwrap();
    let out = dsp.mul(scaled, output_gain).unwrap();
    dsp.finish(out).unwrap()
}

/// The same model as an IR 3 network.
fn network() -> Block {
    let mut w = Weights::random();
    let mut dsp = BlockBuilder::new("wn_network", "WaveNet network", Category::GuitarAmplifier);
    let (_, output_gain) = parameters(&mut dsp);
    let mut net = NetworkBuilder::new();
    let x = net.input();
    net.gain(x, 0);
    let mut input: Buffer = x;
    let mut head: Option<Buffer> = None;
    for array in &ARRAYS {
        let c = array.channels;
        let k = array.kernel;
        let mut v = net.buffer(c);
        net.conv(input, v, 1, 1, w.take(c * array.input), None, false);
        let mut sum = head;
        for &dilation in array.dilations {
            let conv = w.take(c * c * k);
            let conv_bias = w.take(c);
            let mixin = w.take(c);
            let layer = w.take(c * c);
            let layer_bias = w.take(c);
            let z = net.buffer(c);
            net.conv(v, z, k, dilation, conv, Some(conv_bias), false);
            net.conv(x, z, 1, 1, mixin, None, true);
            net.activation(z, Activation::Tanh);
            sum = Some(match sum {
                None => {
                    let s = net.buffer(c);
                    net.copy(z, s);
                    s
                }
                Some(s) => {
                    net.add(z, s);
                    s
                }
            });
            let next = net.buffer(c);
            net.conv(z, next, 1, 1, layer, Some(layer_bias), false);
            net.add(v, next);
            v = next;
        }
        let out = net.buffer(array.head);
        let head_weights = w.take(array.head * c);
        let head_bias = array.head_bias.then(|| w.take(array.head));
        net.conv(sum.unwrap(), out, 1, 1, head_weights, head_bias, false);
        head = Some(out);
        input = v;
    }
    let network = dsp.network(net.finish(head.unwrap())).unwrap();
    let scale = dsp.constant(w.take(1)[0]).unwrap();
    let scaled = dsp.mul(network, scale).unwrap();
    let out = dsp.mul(scaled, output_gain).unwrap();
    dsp.finish(out).unwrap()
}

fn signal(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let t = i as f32 / 48_000.0;
            let level = 0.05 + 0.6 * i as f32 / n as f32;
            level
                * (0.7 * (2.0 * std::f32::consts::PI * 196.0 * t).sin()
                    + 0.3 * (2.0 * std::f32::consts::PI * 523.0 * t).sin())
        })
        .collect()
}

#[test]
fn a_wavenet_network_matches_its_scalar_graph() {
    let (scalar, network) = (scalar(), network());
    assert_eq!(network.version(), 3);
    let back = Block::from_json(&network.to_json().unwrap()).unwrap();
    assert_eq!(back, network, "IR 3 round-trips through JSON");

    let input = signal(4800);
    let mut expected = vec![0.0; input.len()];
    let mut actual = vec![0.0; input.len()];
    let mut a = Processor::new(scalar).unwrap();
    let mut b = Processor::new(network).unwrap();
    for p in [&mut a, &mut b] {
        p.set_parameter(0, 1.7).unwrap();
        p.set_parameter(1, 0.8).unwrap();
    }
    a.process(&input, &mut expected).unwrap();
    b.process(&input, &mut actual).unwrap();
    let peak = expected.iter().fold(0f32, |m, v| m.max(v.abs()));
    let error = expected
        .iter()
        .zip(&actual)
        .fold(0f32, |m, (e, a)| m.max((e - a).abs()));
    assert!(peak > 1e-3, "the model makes a signal: peak {peak}");
    assert!(
        error <= 1e-5 * peak.max(1.0),
        "max error {error}, peak {peak}"
    );

    // Reset clears the history: the second run equals the first.
    b.reset();
    let mut again = vec![0.0; input.len()];
    b.process(&input, &mut again).unwrap();
    assert_eq!(again, actual);
}

#[test]
fn invalid_networks_are_refused() {
    let build = |edit: fn(&mut NetworkBuilder) -> Buffer| {
        let mut dsp = BlockBuilder::new("bad", "Bad", Category::Utility);
        let mut net = NetworkBuilder::new();
        let output = edit(&mut net);
        dsp.network(net.finish(output)).map(|_| ())
    };
    // A buffer read before it is written.
    assert!(
        build(|net| {
            let (a, b) = (net.buffer(1), net.buffer(1));
            net.add(a, b);
            b
        })
        .is_err()
    );
    // A buffer that changes after a convolution read its history.
    assert!(
        build(|net| {
            let (a, b) = (net.buffer(1), net.buffer(1));
            net.copy(net.input(), a);
            net.conv(a, b, 2, 4, vec![0.5, 0.5], None, false);
            net.activation(a, Activation::Relu);
            b
        })
        .is_err()
    );
    // The wrong number of weights, and a gain by a missing parameter.
    assert!(
        build(|net| {
            let b = net.buffer(1);
            net.conv(net.input(), b, 3, 1, vec![1.0], None, false);
            b
        })
        .is_err()
    );
    assert!(
        build(|net| {
            let x = net.input();
            net.gain(x, 0);
            x
        })
        .is_err()
    );
    // The same shapes, in a valid order, build.
    assert!(
        build(|net| {
            let (a, b) = (net.buffer(1), net.buffer(1));
            net.copy(net.input(), a);
            net.conv(a, b, 2, 4, vec![0.5, 0.5], None, false);
            net.activation(b, Activation::Relu);
            b
        })
        .is_ok()
    );
}
