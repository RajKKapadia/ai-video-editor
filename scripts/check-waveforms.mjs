import assert from 'node:assert/strict';
import { connect, until } from './browser-session.mjs';

const browser = await connect(Number(process.env.CHROME_PORT || 9333));
try {
  await browser.send('Page.enable');
  await browser.send('Emulation.setDeviceMetricsOverride', {
    width: 1200, height: 900, deviceScaleFactor: 1, mobile: false,
  });
  await browser.send('Page.navigate', { url: process.env.CUTLINE_URL || 'http://127.0.0.1:1420/' });
  await until(() => browser.evaluate("!!document.querySelector('#root')?.children.length"));
  const ids = await browser.evaluate(`(async () => {
    const {storeMedia} = await import('/src/cache.ts');
    const video = await (await fetch('/tests/fixtures/waveform-tone-silence.mp4')).blob();
    const rate = 8000;
    const wav = new ArrayBuffer(44 + rate * 4 * 2);
    const header = new DataView(wav);
    const tag = (offset, word) => [...word].forEach((char, i) => header.setUint8(offset + i, char.charCodeAt(0)));
    tag(0, 'RIFF'); header.setUint32(4, wav.byteLength - 8, true); tag(8, 'WAVE');
    tag(12, 'fmt '); header.setUint32(16, 16, true); header.setUint16(20, 1, true);
    header.setUint16(22, 1, true); header.setUint32(24, rate, true); header.setUint32(28, rate * 2, true);
    header.setUint16(32, 2, true); header.setUint16(34, 16, true);
    tag(36, 'data'); header.setUint32(40, wav.byteLength - 44, true);
    for (let i = 0; i < rate * 4; i++) {
      if (i < rate || i >= rate * 3)
        header.setInt16(44 + i * 2, Math.round(9000 * Math.sin(i / rate * 440 * 2 * Math.PI)), true);
    }
    const audio = new Blob([wav], {type:'audio/wav'});
    const now = Date.now();
    const clips = [
      {id:crypto.randomUUID(),name:'Tone video',kind:'video',duration:4,size:video.size,addedAt:now,hasAudio:true},
      {id:crypto.randomUUID(),name:'Tone audio',kind:'audio',duration:4,size:audio.size,addedAt:now},
    ];
    await storeMedia(clips[0].id, video);
    await storeMedia(clips[1].id, audio);
    const timeline = [
      {id:'video-trim',clipId:clips[0].id,in:0.5,out:3.5},
      {id:'video-silence',clipId:clips[0].id,in:1.5,out:2.5},
      {id:'standalone',clipId:clips[1].id,in:0,out:4},
    ];
    localStorage.setItem('cutline.projects.v1',JSON.stringify([{id:'waveform-check',name:'Waveforms',description:'',createdAt:now,updatedAt:now,clips,timeline,messages:[]}]));
    location.hash='project=waveform-check';
    return clips.map(clip => clip.id);
  })()`);
  await browser.send('Page.reload');
  const levels = `(() => {
    const canvas = document.querySelector('.timeline-waveforms');
    if (!canvas?.clientWidth) return [];
    const clips = document.querySelectorAll('.timeline-segment.audio');
    if (clips.length !== 3) return [];
    const left = canvas.getBoundingClientRect().left;
    const density = canvas.width / canvas.clientWidth;
    const context = canvas.getContext('2d');
    return [[0, 0.08], [0, 0.5], [0, 0.92], [1, 0.5], [2, 0.1], [2, 0.5], [2, 0.9]].map(([index, fraction]) => {
      const rect = clips[index].getBoundingClientRect();
      const x = Math.floor((rect.left + rect.width * fraction - left) * density);
      const pixel = context.getImageData(x, 0, 1, canvas.height).data;
      return pixel.filter((alpha, i) => i % 4 === 3 && alpha >= 100).length;
    });
  })()`;
  const expected = `(() => {
    const [a,b,c,d,e,f,g] = ${levels};
    return a >= 8 && b <= 2 && c >= 8 && d >= 1 && d <= 2 && e >= 8 && f <= 2 && g >= 8;
  })()`;
  await until(() => browser.evaluate(expected));
  assert.ok(await browser.evaluate(`(async () => {
    const {readCacheEntry} = await import('/src/cache.ts');
    const entries = await Promise.all(${JSON.stringify(ids)}.map(id => readCacheEntry('waveform.v1:'+id)));
    return entries.every(waveform => waveform.secondsPerBin === 0.02 && waveform.peaks.length === 200 &&
      waveform.peaks[24] > 80 && waveform.peaks[100] === 0 && waveform.peaks[175] > 80);
  })()`), 'Cached source peaks preserve tone and silence in both formats');
  await browser.evaluate(`(() => {
    const clip = document.querySelector('.timeline-segment.audio');
    const rect = clip.getBoundingClientRect();
    clip.dispatchEvent(new MouseEvent('click', {bubbles:true,detail:1,clientX:rect.left + rect.width / 2}));
  })()`);
  await until(() => browser.evaluate(
    "Math.abs(Number(document.querySelector('[aria-label=\"Viewer playhead\"]')?.value) - 1.5) < 0.06",
  ));
  await browser.evaluate(`document.querySelector('[aria-label="Timeline zoom"]').focus()`);
  await browser.send('Input.dispatchKeyEvent', {type:'keyDown',key:'End',code:'End',windowsVirtualKeyCode:35});
  await browser.send('Input.dispatchKeyEvent', {type:'keyUp',key:'End',code:'End',windowsVirtualKeyCode:35});
  await browser.evaluate(`(() => {
    const last = document.querySelector('.timeline-track.audio .timeline-segment:last-of-type');
    document.querySelector('.timeline-scroll').scrollLeft = last.offsetLeft - 20;
  })()`);
  await until(() => browser.evaluate(`(() => {
    const canvas = document.querySelector('.timeline-waveforms');
    const rect = canvas.getBoundingClientRect();
    const last = document.querySelector('.timeline-track.audio .timeline-segment:last-of-type').getBoundingClientRect();
    const at = (fraction) => canvas.getContext('2d').getImageData(
      Math.floor(last.left + last.width * fraction - rect.left), 0, 1, canvas.height
    ).data.filter((alpha, i) => i % 4 === 3 && alpha >= 100).length;
    return canvas.width < 1500 && at(0.125) >= 8 && at(0.4) <= 2;
  })()`));
  await browser.send('Page.addScriptToEvaluateOnNewDocument', {
    source: 'window.OfflineAudioContext = class { constructor() { throw new Error("Should use cached peaks"); } }',
  });
  await browser.send('Page.reload');
  await until(() => browser.evaluate(expected));
  console.log('Waveforms passed: real tone and silence, linked and standalone audio, trimmed and scrolled ranges, click to seek, and cached reload.');
} finally {
  browser.close();
}
