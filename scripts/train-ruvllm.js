#!/usr/bin/env node
/**
 * WiFi-DensePose CSI Training Pipeline using ruvllm
 *
 * Complete training, refinement, and quantization pipeline for CSI sensing models.
 * Uses ruvllm's ContrastiveTrainer, TrainingPipeline, LoRA, EWC, and SafeTensors export.
 *
 * Usage:
 *   node scripts/train-ruvllm.js --data data/recordings/pretrain-*.csi.jsonl
 *   node scripts/train-ruvllm.js --data data/recordings/pretrain-1775182186.csi.jsonl --benchmark
 *   node scripts/train-ruvllm.js --data data/recordings/*.csi.jsonl --output models/csi-v1
 *
 * ADR: docs/adr/ADR-071-ruvllm-training-pipeline.md
 */

'use strict';

const fs = require('fs');
const path = require('path');
const { parseArgs } = require('util');

// ---------------------------------------------------------------------------
// Resolve ruvllm from vendor tree — use compiled JS output
// ---------------------------------------------------------------------------
const RUVLLM_PATH = path.resolve(__dirname, '..', 'vendor', 'ruvector', 'npm', 'packages', 'ruvllm', 'src');

const {
  ContrastiveTrainer,
  cosineSimilarity,
  tripletLoss,
  infoNCELoss,
} = require(path.join(RUVLLM_PATH, 'contrastive.js'));

const {
  TrainingPipeline,
} = require(path.join(RUVLLM_PATH, 'training.js'));

const {
  LoraAdapter,
  LoraManager,
} = require(path.join(RUVLLM_PATH, 'lora.js'));

const {
  EwcManager,
  ReasoningBank,
  SonaCoordinator,
} = require(path.join(RUVLLM_PATH, 'sona.js'));

const {
  SafeTensorsWriter,
  ModelExporter,
  DatasetExporter,
} = require(path.join(RUVLLM_PATH, 'export.js'));

// ---------------------------------------------------------------------------
// CLI argument parsing
// ---------------------------------------------------------------------------
const { values: args } = parseArgs({
  options: {
    data: { type: 'string', short: 'd' },
    output: { type: 'string', short: 'o', default: 'models/csi-ruvllm' },
    benchmark: { type: 'boolean', short: 'b', default: false },
    epochs: { type: 'string', short: 'e', default: '20' },
    'batch-size': { type: 'string', default: '32' },
    'lora-rank': { type: 'string', default: '4' },
    'quantize-bits': { type: 'string', default: '4' },
    verbose: { type: 'boolean', short: 'v', default: false },
  },
  strict: true,
});

if (!args.data) {
  console.error('Usage: node scripts/train-ruvllm.js --data <path-to-csi-jsonl> [--output dir] [--benchmark]');
  process.exit(1);
}

const CONFIG = {
  dataGlob: args.data,
  outputDir: args.output,
  benchmark: args.benchmark,
  epochs: parseInt(args.epochs, 10),
  batchSize: parseInt(args['batch-size'], 10),
  loraRank: parseInt(args['lora-rank'], 10),
  quantizeBits: parseInt(args['quantize-bits'], 10),
  verbose: args.verbose,

  // Contrastive training hyperparameters
  margin: 0.3,
  temperature: 0.07,
  hardNegativeRatio: 0.7,
  learningRate: 0.001,

  // Temporal window thresholds (seconds)
  positiveWindowSec: 1.0,
  negativeWindowSec: 30.0,

  // Feature dimensions
  inputDim: 8,        // 8-dim CSI feature vector
  hiddenDim: 64,      // intermediate
  embeddingDim: 128,   // output embedding
};

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------

/**
 * Parse CSI JSONL file into typed frames.
 * Returns arrays of feature frames, vitals frames, and raw CSI frames.
 */
function loadCsiData(filePath) {
  const features = [];
  const vitals = [];
  const rawCsi = [];

  const content = fs.readFileSync(filePath, 'utf-8');
  const lines = content.split('\n').filter(l => l.trim());

  for (const line of lines) {
    try {
      const frame = JSON.parse(line);
      switch (frame.type) {
        case 'feature':
          features.push({
            timestamp: frame.timestamp,
            nodeId: frame.node_id,
            features: frame.features,  // 8-dim float array
            rssi: frame.rssi,
            seq: frame.seq,
          });
          break;
        case 'vitals':
          vitals.push({
            timestamp: frame.timestamp,
            nodeId: frame.node_id,
            breathingBpm: frame.breathing_bpm,
            heartrateBpm: frame.heartrate_bpm,
            nPersons: frame.n_persons,
            motionEnergy: frame.motion_energy,
            presenceScore: frame.presence_score,
            rssi: frame.rssi,
          });
          break;
        case 'raw_csi':
          rawCsi.push({
            timestamp: frame.timestamp,
            nodeId: frame.node_id,
            subcarriers: frame.subcarriers,
            iqHex: frame.iq_hex,
            rssi: frame.rssi,
          });
          break;
      }
    } catch (e) {
      // Skip malformed lines
    }
  }

  return { features, vitals, rawCsi };
}

/**
 * Resolve glob pattern to file list. Handles simple * patterns on both
 * Unix and Windows without requiring a glob library.
 */
function resolveGlob(pattern) {
  if (!pattern.includes('*')) {
    return fs.existsSync(pattern) ? [pattern] : [];
  }
  const dir = path.dirname(pattern);
  const base = path.basename(pattern);
  const regex = new RegExp('^' + base.replace(/\*/g, '.*') + '$');
  if (!fs.existsSync(dir)) return [];
  return fs.readdirSync(dir)
    .filter(f => regex.test(f))
    .map(f => path.join(dir, f));
}

// ---------------------------------------------------------------------------
// Embedding encoder (simulates 8 -> 64 -> 128 FC network)
// ---------------------------------------------------------------------------

/**
 * Simple two-layer FC encoder: 8 -> 64 -> 128
 * Uses deterministic seeded weights for reproducibility.
 */
class CsiEncoder {
  constructor(inputDim, hiddenDim, outputDim, seed = 42) {
    this.inputDim = inputDim;
    this.hiddenDim = hiddenDim;
    this.outputDim = outputDim;

    // Initialize weights with seeded pseudo-random values (Kaiming)
    const rng = this._createRng(seed);
    this.w1 = this._initMatrix(inputDim, hiddenDim, rng, inputDim);
    this.b1 = new Float64Array(hiddenDim);
    this.w2 = this._initMatrix(hiddenDim, outputDim, rng, hiddenDim);
    this.b2 = new Float64Array(outputDim);
  }

  /**
   * Forward pass: input (8-dim) -> embedding (128-dim)
   */
  encode(input) {
    // Layer 1: input @ w1 + b1, then ReLU
    const hidden = new Float64Array(this.hiddenDim);
    for (let j = 0; j < this.hiddenDim; j++) {
      let sum = this.b1[j];
      for (let i = 0; i < this.inputDim; i++) {
        sum += (input[i] || 0) * this.w1[i * this.hiddenDim + j];
      }
      hidden[j] = Math.max(0, sum); // ReLU
    }

    // Layer 2: hidden @ w2 + b2
    const output = new Float64Array(this.outputDim);
    for (let j = 0; j < this.outputDim; j++) {
      let sum = this.b2[j];
      for (let i = 0; i < this.hiddenDim; i++) {
        sum += hidden[i] * this.w2[i * this.outputDim + j];
      }
      output[j] = sum;
    }

    // L2 normalize
    let norm = 0;
    for (let i = 0; i < output.length; i++) norm += output[i] * output[i];
    norm = Math.sqrt(norm) || 1;
    const result = new Array(this.outputDim);
    for (let i = 0; i < this.outputDim; i++) result[i] = output[i] / norm;
    return result;
  }

  /**
   * Encode a batch of inputs.
   */
  encodeBatch(inputs) {
    return inputs.map(input => this.encode(input));
  }

  _createRng(seed) {
    // Simple xorshift32 PRNG
    let s = seed;
    return () => {
      s ^= s << 13;
      s ^= s >> 17;
      s ^= s << 5;
      return ((s >>> 0) / 4294967296) - 0.5;
    };
  }

  _initMatrix(rows, cols, rng, fanIn) {
    const scale = Math.sqrt(2.0 / fanIn);
    const arr = new Float64Array(rows * cols);
    for (let i = 0; i < arr.length; i++) {
      arr[i] = rng() * scale;
    }
    return arr;
  }
}

// ---------------------------------------------------------------------------
// Triplet generation
// ---------------------------------------------------------------------------

/**
 * Generate contrastive triplets from feature frames.
 *
 * Strategies:
 * 1. Temporal positive: frames within 1s = similar environment state
 * 2. Temporal negative: frames >30s apart = different state
 * 3. Cross-node positive: same timestamp from node 1 and node 2 = same person
 * 4. Cross-node negative: different timestamp, different node = different state
 * 5. Hard negatives: frames near transition boundaries
 */
function generateTriplets(features, vitals, config) {
  const triplets = [];

  // Index features by node
  const byNode = {};
  for (const f of features) {
    if (!byNode[f.nodeId]) byNode[f.nodeId] = [];
    byNode[f.nodeId].push(f);
  }
  const nodeIds = Object.keys(byNode).map(Number);

  // Sort each node's features by timestamp
  for (const nid of nodeIds) {
    byNode[nid].sort((a, b) => a.timestamp - b.timestamp);
  }

  // Build a timestamp -> vitals map for labeling
  const vitalsMap = new Map();
  for (const v of vitals) {
    const key = `${v.nodeId}-${Math.round(v.timestamp * 10)}`;
    vitalsMap.set(key, v);
  }

  function findNearestVitals(nodeId, timestamp) {
    // Simple nearest-neighbor lookup in vitals
    let best = null;
    let bestDist = Infinity;
    for (const v of vitals) {
      if (v.nodeId !== nodeId) continue;
      const dist = Math.abs(v.timestamp - timestamp);
      if (dist < bestDist) {
        bestDist = dist;
        best = v;
      }
    }
    return best;
  }

  // Strategy 1 + 2: Temporal positive/negative within same node
  for (const nid of nodeIds) {
    const frames = byNode[nid];
    for (let i = 0; i < frames.length; i++) {
      const anchor = frames[i];

      // Find temporal positive (within 1 second)
      for (let j = i + 1; j < frames.length && j < i + 20; j++) {
        const candidate = frames[j];
        const timeDiff = Math.abs(candidate.timestamp - anchor.timestamp);

        if (timeDiff <= config.positiveWindowSec) {
          // Find a temporal negative (>30 seconds away)
          for (let k = 0; k < frames.length; k++) {
            const neg = frames[k];
            const negTimeDiff = Math.abs(neg.timestamp - anchor.timestamp);

            if (negTimeDiff >= config.negativeWindowSec) {
              const isHard = negTimeDiff < config.negativeWindowSec * 2;
              triplets.push({
                anchor: anchor.features,
                positive: candidate.features,
                negative: neg.features,
                isHard,
                type: 'temporal',
                anchorLabel: `node${nid}-t${anchor.timestamp.toFixed(2)}`,
                posLabel: `node${nid}-t${candidate.timestamp.toFixed(2)}`,
                negLabel: `node${nid}-t${neg.timestamp.toFixed(2)}`,
              });
              break; // One negative per positive
            }
          }
        }
      }
    }
  }

  // Strategy 3: Cross-node positive (same timestamp, different nodes)
  if (nodeIds.length >= 2) {
    const node1Frames = byNode[nodeIds[0]] || [];
    const node2Frames = byNode[nodeIds[1]] || [];

    for (const f1 of node1Frames) {
      // Find node2 frame closest in time
      let bestMatch = null;
      let bestDist = Infinity;
      for (const f2 of node2Frames) {
        const dist = Math.abs(f2.timestamp - f1.timestamp);
        if (dist < bestDist) {
          bestDist = dist;
          bestMatch = f2;
        }
      }

      if (bestMatch && bestDist < config.positiveWindowSec) {
        // Find a cross-node negative (different time from different node)
        for (const f2neg of node2Frames) {
          const negDist = Math.abs(f2neg.timestamp - f1.timestamp);
          if (negDist >= config.negativeWindowSec) {
            triplets.push({
              anchor: f1.features,
              positive: bestMatch.features,
              negative: f2neg.features,
              isHard: false,
              type: 'cross-node',
              anchorLabel: `node${f1.nodeId}-t${f1.timestamp.toFixed(2)}`,
              posLabel: `node${bestMatch.nodeId}-t${bestMatch.timestamp.toFixed(2)}`,
              negLabel: `node${f2neg.nodeId}-t${f2neg.timestamp.toFixed(2)}`,
            });
            break;
          }
        }
      }
    }
  }

  // Strategy 5: Hard negatives near scenario transitions
  // Detect transitions via motion_energy spikes in vitals
  const sortedVitals = [...vitals].sort((a, b) => a.timestamp - b.timestamp);
  const transitionTimes = [];
  for (let i = 1; i < sortedVitals.length; i++) {
    const prev = sortedVitals[i - 1];
    const curr = sortedVitals[i];
    const energyDelta = Math.abs(curr.motionEnergy - prev.motionEnergy);
    if (energyDelta > 2.0) {
      transitionTimes.push(curr.timestamp);
    }
  }

  // Add hard negatives from transition boundaries
  for (const transTime of transitionTimes.slice(0, 50)) {
    for (const nid of nodeIds) {
      const frames = byNode[nid];
      // Find frame just before and just after transition
      let before = null, after = null;
      for (const f of frames) {
        if (f.timestamp < transTime) before = f;
        if (f.timestamp > transTime && !after) after = f;
      }
      if (before && after) {
        // The first non-transition frame as anchor
        const anchorIdx = Math.max(0, frames.indexOf(before) - 5);
        const anchor = frames[anchorIdx];
        if (anchor) {
          triplets.push({
            anchor: anchor.features,
            positive: before.features,
            negative: after.features,
            isHard: true,
            type: 'transition-hard',
            anchorLabel: `node${nid}-pre-transition`,
            posLabel: `node${nid}-before`,
            negLabel: `node${nid}-after`,
          });
        }
      }
    }
  }

  return triplets;
}

// ---------------------------------------------------------------------------
// Quantization (TurboQuant simulation)
// ---------------------------------------------------------------------------

/**
 * Quantize Float32Array to N-bit fixed point.
 * Returns { quantized: Uint8Array, scale: number, zeroPoint: number }.
 * Compression ratio: 32 / bits.
 */
function quantizeWeights(weights, bits) {
  const maxVal = 2 ** (bits - 1) - 1;
  const minVal = -(2 ** (bits - 1));

  let wMin = Infinity, wMax = -Infinity;
  for (let i = 0; i < weights.length; i++) {
    if (weights[i] < wMin) wMin = weights[i];
    if (weights[i] > wMax) wMax = weights[i];
  }

  const scale = (wMax - wMin) / (maxVal - minVal) || 1e-10;
  const zeroPoint = Math.round(-wMin / scale + minVal);

  // Pack into bytes (simplified — store one value per byte for 4-bit/8-bit)
  const bytesPerWeight = bits <= 8 ? 1 : 2;
  const quantized = new Uint8Array(weights.length * bytesPerWeight);

  for (let i = 0; i < weights.length; i++) {
    let q = Math.round(weights[i] / scale) + zeroPoint;
    q = Math.max(minVal, Math.min(maxVal, q));
    quantized[i] = (q - minVal) & 0xFF;
  }

  return {
    quantized,
    scale,
    zeroPoint,
    bits,
    originalSize: weights.length * 4,  // fp32 bytes
    quantizedSize: quantized.length,
    compressionRatio: (weights.length * 4) / quantized.length,
  };
}

/**
 * Dequantize back to float for quality assessment.
 */
function dequantizeWeights(quantized, scale, zeroPoint, bits) {
  const minVal = -(2 ** (bits - 1));
  const result = new Float32Array(quantized.length);
  for (let i = 0; i < quantized.length; i++) {
    const q = (quantized[i] + minVal) - zeroPoint;
    result[i] = q * scale;
  }
  return result;
}

/**
 * Compute quantization quality loss (RMSE between original and dequantized).
 */
function quantizationQuality(original, dequantized) {
  let sumSqErr = 0;
  const n = Math.min(original.length, dequantized.length);
  for (let i = 0; i < n; i++) {
    const diff = original[i] - dequantized[i];
    sumSqErr += diff * diff;
  }
  return Math.sqrt(sumSqErr / n);
}

// ---------------------------------------------------------------------------
// Training labels from vitals data
// ---------------------------------------------------------------------------

/**
 * Create task-head labels from vitals data for each feature frame.
 * Returns { presence: number, activity: number[], vitalsTarget: number[] }
 */
function createLabels(featureFrame, vitals) {
  // Find nearest vitals for this frame
  let nearest = null;
  let bestDist = Infinity;
  for (const v of vitals) {
    if (v.nodeId !== featureFrame.nodeId) continue;
    const dist = Math.abs(v.timestamp - featureFrame.timestamp);
    if (dist < bestDist) {
      bestDist = dist;
      nearest = v;
    }
  }

  if (!nearest || bestDist > 2.0) {
    return null; // No matching vitals within 2 seconds
  }

  // Presence: binary (threshold at 0.3)
  const presence = nearest.presenceScore > 0.3 ? 1.0 : 0.0;

  // Activity: [still, moving, empty] as one-hot
  let activity;
  if (nearest.presenceScore <= 0.1) {
    activity = [0, 0, 1]; // empty
  } else if (nearest.motionEnergy > 2.0) {
    activity = [0, 1, 0]; // moving
  } else {
    activity = [1, 0, 0]; // still
  }

  // Vitals: [breathing BPM normalized, heartrate BPM normalized]
  const vitalsTarget = [
    nearest.breathingBpm / 30.0,   // normalize to ~0-1 range
    nearest.heartrateBpm / 120.0,  // normalize to ~0-1 range
  ];

  return { presence, activity, vitalsTarget };
}

// ---------------------------------------------------------------------------
// Main pipeline
// ---------------------------------------------------------------------------

async function main() {
  const startTime = Date.now();
  console.log('=== WiFi-DensePose CSI Training Pipeline (ruvllm) ===');
  console.log(`Config: epochs=${CONFIG.epochs} batch=${CONFIG.batchSize} lora_rank=${CONFIG.loraRank} quant=${CONFIG.quantizeBits}bit`);
  console.log('');

  // -----------------------------------------------------------------------
  // Step 1: Load CSI data
  // -----------------------------------------------------------------------
  console.log('[1/9] Loading CSI data...');
  const files = resolveGlob(CONFIG.dataGlob);
  if (files.length === 0) {
    console.error(`No files found matching: ${CONFIG.dataGlob}`);
    process.exit(1);
  }

  let allFeatures = [];
  let allVitals = [];
  let allRawCsi = [];

  for (const file of files) {
    console.log(`  Loading: ${path.basename(file)}`);
    const { features, vitals, rawCsi } = loadCsiData(file);
    allFeatures = allFeatures.concat(features);
    allVitals = allVitals.concat(vitals);
    allRawCsi = allRawCsi.concat(rawCsi);
  }

  console.log(`  Loaded: ${allFeatures.length} features, ${allVitals.length} vitals, ${allRawCsi.length} raw CSI frames`);
  console.log(`  Nodes: ${[...new Set(allFeatures.map(f => f.nodeId))].join(', ')}`);

  if (allFeatures.length === 0) {
    console.error('No feature frames found in data. Ensure data contains type="feature" frames.');
    process.exit(1);
  }

  // -----------------------------------------------------------------------
  // Step 2: Generate contrastive triplets
  // -----------------------------------------------------------------------
  console.log('\n[2/9] Generating contrastive triplets...');
  const triplets = generateTriplets(allFeatures, allVitals, CONFIG);

  const temporalCount = triplets.filter(t => t.type === 'temporal').length;
  const crossNodeCount = triplets.filter(t => t.type === 'cross-node').length;
  const hardCount = triplets.filter(t => t.isHard).length;

  console.log(`  Total triplets: ${triplets.length}`);
  console.log(`  Temporal: ${temporalCount}, Cross-node: ${crossNodeCount}, Hard: ${hardCount}`);
  console.log(`  Hard negative ratio: ${(hardCount / triplets.length * 100).toFixed(1)}%`);

  if (triplets.length === 0) {
    console.error('No triplets generated. Data may lack temporal diversity (need >30s span).');
    process.exit(1);
  }

  // -----------------------------------------------------------------------
  // Step 3: Build encoder and encode features
  // -----------------------------------------------------------------------
  console.log('\n[3/9] Building CSI encoder (8 -> 64 -> 128)...');
  const encoder = new CsiEncoder(CONFIG.inputDim, CONFIG.hiddenDim, CONFIG.embeddingDim);

  // Pre-encode all features
  console.log('  Encoding feature vectors...');
  const encodingStart = Date.now();
  const encodedFeatures = allFeatures.map(f => ({
    ...f,
    embedding: encoder.encode(f.features),
  }));
  console.log(`  Encoded ${encodedFeatures.length} frames in ${Date.now() - encodingStart}ms`);

  // -----------------------------------------------------------------------
  // Phase 1: Contrastive pretraining
  // -----------------------------------------------------------------------
  console.log('\n[4/9] Phase 1: Contrastive pretraining...');
  const contrastiveTrainer = new ContrastiveTrainer({
    epochs: CONFIG.epochs,
    batchSize: CONFIG.batchSize,
    margin: CONFIG.margin,
    temperature: CONFIG.temperature,
    hardNegativeRatio: CONFIG.hardNegativeRatio,
    learningRate: CONFIG.learningRate,
    outputPath: path.join(CONFIG.outputDir, 'contrastive'),
  });

  // Add triplets with encoded embeddings
  for (const triplet of triplets) {
    const anchorEmb = encoder.encode(triplet.anchor);
    const posEmb = encoder.encode(triplet.positive);
    const negEmb = encoder.encode(triplet.negative);

    contrastiveTrainer.addTriplet(
      triplet.anchorLabel,
      anchorEmb,
      triplet.posLabel,
      posEmb,
      triplet.negLabel,
      negEmb,
      triplet.isHard
    );
  }

  console.log(`  Triplets loaded: ${contrastiveTrainer.getTripletCount()}`);
  const contrastiveResult = contrastiveTrainer.train();
  console.log(`  Epochs: ${contrastiveResult.history.length}`);
  console.log(`  Initial loss: ${contrastiveResult.initialLoss.toFixed(6)}`);
  console.log(`  Final loss: ${contrastiveResult.finalLoss.toFixed(6)}`);
  console.log(`  Improvement: ${contrastiveResult.improvement.toFixed(1)}%`);
  console.log(`  Duration: ${contrastiveResult.durationMs}ms`);

  // Export contrastive training data
  const contrastiveOutDir = contrastiveTrainer.exportTrainingData();
  console.log(`  Training data exported to: ${contrastiveOutDir}`);

  // -----------------------------------------------------------------------
  // Phase 2: Task head training via TrainingPipeline
  // -----------------------------------------------------------------------
  console.log('\n[5/9] Phase 2: Task head training...');

  // Create LoRA adapter for the task heads: 128-dim input, 128-dim output
  const taskAdapter = new LoraAdapter(
    { rank: CONFIG.loraRank * 2, alpha: CONFIG.loraRank * 4, dropout: 0.05, targetModules: ['encoder', 'task_heads'] },
    CONFIG.embeddingDim,
    CONFIG.embeddingDim
  );

  const taskPipeline = new TrainingPipeline({
    learningRate: CONFIG.learningRate,
    batchSize: CONFIG.batchSize,
    epochs: Math.max(5, Math.floor(CONFIG.epochs / 2)),
    scheduler: 'cosine',
    warmupSteps: 50,
    earlyStoppingPatience: 5,
    checkpointInterval: 2,
    ewcLambda: 2000,
    validationSplit: 0.1,
  }, taskAdapter);

  // Build training data: input = encoded feature, target = task labels
  let labeledCount = 0;
  const taskTrainingData = [];

  for (const ef of encodedFeatures) {
    const labels = createLabels(ef, allVitals);
    if (!labels) continue;

    // Construct target vector: [presence(1), activity(3), vitals(2), padding(122)]
    // Total: 128-dim to match adapter output dim
    const target = new Array(CONFIG.embeddingDim).fill(0);
    target[0] = labels.presence;
    target[1] = labels.activity[0]; // still
    target[2] = labels.activity[1]; // moving
    target[3] = labels.activity[2]; // empty
    target[4] = labels.vitalsTarget[0]; // breathing normalized
    target[5] = labels.vitalsTarget[1]; // heartrate normalized

    taskTrainingData.push({
      input: ef.embedding,
      target,
      quality: 1.0,
    });
    labeledCount++;
  }

  console.log(`  Labeled samples: ${labeledCount} / ${encodedFeatures.length} (${(labeledCount / encodedFeatures.length * 100).toFixed(1)}%)`);

  if (taskTrainingData.length > 0) {
    taskPipeline.addData(taskTrainingData);
    const taskResult = taskPipeline.train();

    console.log(`  Epochs completed: ${taskResult.epochs}`);
    console.log(`  Final loss: ${taskResult.finalLoss.toFixed(6)}`);
    console.log(`  Best val loss: ${taskResult.bestValLoss.toFixed(6)}`);
    console.log(`  Early stopped: ${taskResult.earlyStopped}`);
    console.log(`  Duration: ${taskResult.durationMs}ms`);
  } else {
    console.log('  WARN: No labeled data available, skipping task head training.');
  }

  // -----------------------------------------------------------------------
  // Phase 3: LoRA refinement (per-node room adaptation)
  // -----------------------------------------------------------------------
  console.log('\n[6/9] Phase 3: LoRA refinement (per-node adaptation)...');
  const loraManager = new LoraManager({
    rank: CONFIG.loraRank,
    alpha: CONFIG.loraRank * 2,
    dropout: 0.1,
    targetModules: ['room_adapt'],
  });

  const nodeIds = [...new Set(allFeatures.map(f => f.nodeId))];

  for (const nodeId of nodeIds) {
    console.log(`  Training LoRA adapter for node ${nodeId}...`);
    const nodeAdapter = loraManager.create(
      `node-${nodeId}`,
      { rank: CONFIG.loraRank, alpha: CONFIG.loraRank * 2, dropout: 0.1 },
      CONFIG.embeddingDim,
      CONFIG.embeddingDim
    );

    // Train on node-specific data
    const nodeFeatures = encodedFeatures.filter(f => f.nodeId === nodeId);
    const nodePipeline = new TrainingPipeline({
      learningRate: CONFIG.learningRate * 0.5,
      batchSize: Math.min(CONFIG.batchSize, nodeFeatures.length),
      epochs: 5,
      scheduler: 'cosine',
      ewcLambda: 3000,
    }, nodeAdapter);

    const nodeData = [];
    for (const nf of nodeFeatures) {
      const labels = createLabels(nf, allVitals);
      if (!labels) continue;
      const target = new Array(CONFIG.embeddingDim).fill(0);
      target[0] = labels.presence;
      target[1] = labels.activity[0];
      target[2] = labels.activity[1];
      target[3] = labels.activity[2];
      target[4] = labels.vitalsTarget[0];
      target[5] = labels.vitalsTarget[1];
      nodeData.push({ input: nf.embedding, target, quality: 1.0 });
    }

    if (nodeData.length > 0) {
      nodePipeline.addData(nodeData);
      const nodeResult = nodePipeline.train();
      console.log(`    Node ${nodeId}: ${nodeData.length} samples, loss=${nodeResult.finalLoss.toFixed(6)}, ${nodeResult.durationMs}ms`);
    }
  }

  console.log(`  LoRA adapters: ${loraManager.list().join(', ')}`);
  console.log(`  Total LoRA parameters: ${loraManager.stats().totalParameters}`);

  // -----------------------------------------------------------------------
  // Phase 4: Quantization (TurboQuant)
  // -----------------------------------------------------------------------
  console.log('\n[7/9] Phase 4: Quantization (TurboQuant)...');
  const mergedWeights = taskAdapter.merge();
  const flatWeights = new Float32Array(mergedWeights.flat());

  const quantResults = {};
  for (const bits of [2, 4, 8]) {
    const qr = quantizeWeights(flatWeights, bits);
    const deq = dequantizeWeights(qr.quantized, qr.scale, qr.zeroPoint, bits);
    const rmse = quantizationQuality(flatWeights, deq);
    quantResults[bits] = { ...qr, rmse };
    console.log(`  ${bits}-bit: compression=${qr.compressionRatio.toFixed(1)}x, RMSE=${rmse.toFixed(6)}, size=${(qr.quantizedSize / 1024).toFixed(1)}KB`);
  }

  // -----------------------------------------------------------------------
  // Phase 5: EWC consolidation
  // -----------------------------------------------------------------------
  console.log('\n[8/9] Phase 5: EWC consolidation...');
  const ewcManager = taskPipeline.getEwcManager();
  const ewcWeights = taskAdapter.merge().flat();
  ewcManager.registerTask('csi-pretraining-v1', ewcWeights);

  // Register per-node tasks for EWC protection
  for (const nodeId of nodeIds) {
    const nodeAdapter = loraManager.get(`node-${nodeId}`);
    if (nodeAdapter) {
      const nodeWeights = nodeAdapter.merge().flat();
      ewcManager.registerTask(`node-${nodeId}-adaptation`, nodeWeights);
    }
  }

  const ewcStats = ewcManager.stats();
  console.log(`  Tasks learned: ${ewcStats.tasksLearned}`);
  console.log(`  Fisher computed: ${ewcStats.fisherComputed}`);
  console.log(`  Protection strength: ${ewcStats.protectionStrength}`);
  console.log(`  Forgetting rate: ${ewcStats.forgettingRate.toFixed(4)}`);

  // -----------------------------------------------------------------------
  // Step 9: Export
  // -----------------------------------------------------------------------
  console.log('\n[9/9] Exporting models...');

  // Ensure output directory exists
  fs.mkdirSync(CONFIG.outputDir, { recursive: true });

  // 9a: SafeTensors export via ModelExporter
  const exporter = new ModelExporter();
  const exportModel = {
    metadata: {
      name: 'wifi-densepose-csi-embedding',
      version: '1.0.0',
      architecture: 'csi-encoder-8-64-128',
      training: {
        steps: contrastiveResult.history.length * contrastiveTrainer.getTripletCount(),
        loss: contrastiveResult.finalLoss,
        learningRate: CONFIG.learningRate,
      },
      custom: {
        inputDim: CONFIG.inputDim,
        hiddenDim: CONFIG.hiddenDim,
        embeddingDim: CONFIG.embeddingDim,
        totalFrames: allFeatures.length,
        totalTriplets: triplets.length,
        nodes: nodeIds,
        quantizationBits: CONFIG.quantizeBits,
      },
    },
    loraWeights: taskAdapter.getWeights(),
    loraConfig: taskAdapter.getConfig(),
    ewcStats: ewcStats,
    tensors: new Map(),
  };

  // Add encoder weights as tensors
  exportModel.tensors.set('encoder.w1', new Float32Array(encoder.w1));
  exportModel.tensors.set('encoder.b1', new Float32Array(encoder.b1));
  exportModel.tensors.set('encoder.w2', new Float32Array(encoder.w2));
  exportModel.tensors.set('encoder.b2', new Float32Array(encoder.b2));

  // SafeTensors
  const safetensorsBuffer = exporter.toSafeTensors(exportModel);
  fs.writeFileSync(path.join(CONFIG.outputDir, 'model.safetensors'), safetensorsBuffer);
  console.log(`  SafeTensors: ${path.join(CONFIG.outputDir, 'model.safetensors')} (${(safetensorsBuffer.length / 1024).toFixed(1)} KB)`);

  // HuggingFace export
  const hfExport = exporter.toHuggingFace(exportModel);
  fs.writeFileSync(path.join(CONFIG.outputDir, 'config.json'), hfExport.config);
  console.log(`  HF config: ${path.join(CONFIG.outputDir, 'config.json')}`);

  // JSON export
  const jsonExport = exporter.toJSON(exportModel);
  fs.writeFileSync(path.join(CONFIG.outputDir, 'model.json'), jsonExport);

  // 9b: Quantized models
  const quantDir = path.join(CONFIG.outputDir, 'quantized');
  fs.mkdirSync(quantDir, { recursive: true });

  for (const [bits, qr] of Object.entries(quantResults)) {
    const qPath = path.join(quantDir, `model-q${bits}.bin`);
    fs.writeFileSync(qPath, Buffer.from(qr.quantized));
    console.log(`  Quantized ${bits}-bit: ${qPath} (${(qr.quantizedSize / 1024).toFixed(1)} KB)`);
  }

  // 9c: Per-node LoRA adapters
  const loraDir = path.join(CONFIG.outputDir, 'lora');
  fs.mkdirSync(loraDir, { recursive: true });

  for (const adapterId of loraManager.list()) {
    const adapter = loraManager.get(adapterId);
    const loraPath = path.join(loraDir, `${adapterId}.json`);
    fs.writeFileSync(loraPath, adapter.toJSON());
    console.log(`  LoRA adapter: ${loraPath}`);
  }

  // 9d: RVF (RuVector Format) — JSONL for Cognitum Seed ingest
  const rvfPath = path.join(CONFIG.outputDir, 'model.rvf.jsonl');
  const rvfLines = [
    JSON.stringify({ type: 'metadata', ...exportModel.metadata }),
    JSON.stringify({ type: 'encoder', w1_shape: [CONFIG.inputDim, CONFIG.hiddenDim], w2_shape: [CONFIG.hiddenDim, CONFIG.embeddingDim] }),
    JSON.stringify({ type: 'lora', config: taskAdapter.getConfig(), parameters: taskAdapter.numParameters() }),
    JSON.stringify({ type: 'ewc', stats: ewcStats }),
    JSON.stringify({ type: 'quantization', default_bits: CONFIG.quantizeBits, variants: Object.keys(quantResults).map(Number) }),
  ];
  fs.writeFileSync(rvfPath, rvfLines.join('\n'));
  console.log(`  RVF manifest: ${rvfPath}`);

  // 9e: Training metrics
  const metricsPath = path.join(CONFIG.outputDir, 'training-metrics.json');
  const metrics = {
    timestamp: new Date().toISOString(),
    totalDurationMs: Date.now() - startTime,
    data: {
      files: files.map(f => path.basename(f)),
      totalFeatures: allFeatures.length,
      totalVitals: allVitals.length,
      totalRawCsi: allRawCsi.length,
      nodes: nodeIds,
    },
    contrastive: {
      triplets: triplets.length,
      temporal: temporalCount,
      crossNode: crossNodeCount,
      hardNegatives: hardCount,
      initialLoss: contrastiveResult.initialLoss,
      finalLoss: contrastiveResult.finalLoss,
      improvement: contrastiveResult.improvement,
      durationMs: contrastiveResult.durationMs,
      lossHistory: contrastiveResult.history,
    },
    taskHeads: taskTrainingData.length > 0 ? {
      samples: labeledCount,
      finalLoss: taskPipeline.getMetrics().trainLoss,
    } : null,
    lora: {
      adapters: loraManager.list(),
      totalParameters: loraManager.stats().totalParameters,
    },
    quantization: Object.fromEntries(
      Object.entries(quantResults).map(([bits, qr]) => [
        `q${bits}`,
        { compressionRatio: qr.compressionRatio, rmse: qr.rmse, sizeKB: qr.quantizedSize / 1024 },
      ])
    ),
    ewc: ewcStats,
    config: CONFIG,
  };
  fs.writeFileSync(metricsPath, JSON.stringify(metrics, null, 2));
  console.log(`  Metrics: ${metricsPath}`);

  // -----------------------------------------------------------------------
  // Summary
  // -----------------------------------------------------------------------
  const totalDuration = Date.now() - startTime;
  console.log('\n=== Training Complete ===');
  console.log(`  Total duration: ${(totalDuration / 1000).toFixed(1)}s`);
  console.log(`  Output directory: ${path.resolve(CONFIG.outputDir)}`);
  console.log(`  Model size (fp32): ${(safetensorsBuffer.length / 1024).toFixed(1)} KB`);
  console.log(`  Model size (q${CONFIG.quantizeBits}): ${(quantResults[CONFIG.quantizeBits]?.quantizedSize / 1024 || 0).toFixed(1)} KB`);
  console.log(`  LoRA adapters: ${loraManager.count()}`);
  console.log(`  EWC tasks protected: ${ewcStats.tasksLearned}`);

  // -----------------------------------------------------------------------
  // Optional benchmark
  // -----------------------------------------------------------------------
  if (CONFIG.benchmark) {
    console.log('\n=== Benchmark Mode ===');
    runBenchmark(encoder, taskAdapter, allFeatures, allVitals, quantResults);
  }
}

// ---------------------------------------------------------------------------
// Benchmark
// ---------------------------------------------------------------------------
function runBenchmark(encoder, adapter, features, vitals, quantResults) {
  const N = Math.min(1000, features.length);
  const testFeatures = features.slice(0, N);

  // Inference latency
  console.log(`\nInference latency (${N} samples):`);
  const latencies = [];
  for (const f of testFeatures) {
    const start = process.hrtime.bigint();
    const emb = encoder.encode(f.features);
    adapter.forward(emb);
    const elapsed = Number(process.hrtime.bigint() - start) / 1e6;
    latencies.push(elapsed);
  }

  latencies.sort((a, b) => a - b);
  const mean = latencies.reduce((a, b) => a + b, 0) / latencies.length;
  const p95 = latencies[Math.floor(latencies.length * 0.95)];
  const p99 = latencies[Math.floor(latencies.length * 0.99)];

  console.log(`  Mean:  ${mean.toFixed(3)}ms`);
  console.log(`  P95:   ${p95.toFixed(3)}ms`);
  console.log(`  P99:   ${p99.toFixed(3)}ms`);
  console.log(`  Throughput: ${(1000 / mean).toFixed(0)} embeddings/sec`);

  // Embedding quality: cosine similarity for temporal pairs
  console.log('\nEmbedding quality (temporal pairs):');
  let posSimilarities = [];
  let negSimilarities = [];

  for (let i = 0; i < Math.min(features.length - 1, 200); i++) {
    const f1 = features[i];
    const f2 = features[i + 1];
    const timeDiff = Math.abs(f2.timestamp - f1.timestamp);

    const emb1 = encoder.encode(f1.features);
    const emb2 = encoder.encode(f2.features);
    const sim = cosineSimilarity(emb1, emb2);

    if (timeDiff <= 1.0) {
      posSimilarities.push(sim);
    } else if (timeDiff >= 30.0) {
      negSimilarities.push(sim);
    }
  }

  if (posSimilarities.length > 0) {
    const avgPos = posSimilarities.reduce((a, b) => a + b, 0) / posSimilarities.length;
    console.log(`  Positive pair avg similarity: ${avgPos.toFixed(4)} (n=${posSimilarities.length})`);
  }
  if (negSimilarities.length > 0) {
    const avgNeg = negSimilarities.reduce((a, b) => a + b, 0) / negSimilarities.length;
    console.log(`  Negative pair avg similarity: ${avgNeg.toFixed(4)} (n=${negSimilarities.length})`);
  }

  // Presence detection accuracy
  console.log('\nPresence detection accuracy:');
  let correct = 0, total = 0;
  for (const f of testFeatures) {
    const labels = createLabels(f, vitals);
    if (!labels) continue;

    const emb = encoder.encode(f.features);
    const out = adapter.forward(emb);
    const predicted = out[0] > 0.5 ? 1 : 0;
    if (predicted === labels.presence) correct++;
    total++;
  }
  if (total > 0) {
    console.log(`  Accuracy: ${(correct / total * 100).toFixed(1)}% (${correct}/${total})`);
  }

  // Memory usage per quantization level
  console.log('\nMemory usage per quantization level:');
  console.log('  Bits | Size (KB) | Compression | RMSE');
  console.log('  -----|-----------|-------------|------');
  for (const [bits, qr] of Object.entries(quantResults)) {
    console.log(`  ${bits.padStart(4)} | ${(qr.quantizedSize / 1024).toFixed(1).padStart(9)} | ${qr.compressionRatio.toFixed(1).padStart(11)}x | ${qr.rmse.toFixed(6)}`);
  }
  console.log(`  fp32 | ${(quantResults[Object.keys(quantResults)[0]].originalSize / 1024).toFixed(1).padStart(9)} | ${' '.padStart(10)}1x | 0.000000`);
}

// Run
main().catch(err => {
  console.error('Training pipeline failed:', err);
  process.exit(1);
});
