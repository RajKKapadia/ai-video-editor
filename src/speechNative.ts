export const whisperModels = {
  small: 'Whisper Small',
  medium: 'Whisper Medium',
  largeV3Turbo: 'Whisper Large v3 Turbo',
  largeV3: 'Whisper Large v3',
} as const;

export type WhisperModel = keyof typeof whisperModels;
export type SpeechConfig = {
  preference: 'autoBalanced' | 'autoAccuracy' | WhisperModel;
  processing: 'auto' | 'cpu';
};
export type SpeechDevice = {
  id: number;
  name: string;
  freeMemory: number;
  totalMemory: number;
  sharedMemory: boolean;
};
export type SpeechSelection = {
  model: WhisperModel;
  device: SpeechDevice | null;
  threads: number;
  reason: string;
  benchmark: {
    realtimeFactor: number;
    loadSeconds: number;
    warmupSeconds?: number;
    measuredAtMs: number;
  } | null;
};
export type SpeechStatus = {
  config: SpeechConfig;
  hardware: {
    cpu: string;
    cores: number;
    totalMemory: number;
    availableMemory: number;
    freeDisk: number | null;
    devices: SpeechDevice[];
    compiledBackends: string[];
    warning: string | null;
  };
  selection: SpeechSelection;
  models: { id: WhisperModel; label: string; downloadBytes: number; downloaded: boolean }[];
};

export function gibibytes(bytes: number) {
  return (bytes / 1024 ** 3).toFixed(1) + ' GiB';
}
