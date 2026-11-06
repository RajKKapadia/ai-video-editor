import { connect, until, delay } from './browser-session.mjs';
const browser = await connect(Number(process.env.CHROME_PORT || 9333));
try {
  await browser.send('Page.enable');
  await browser.send('Page.navigate', { url: 'http://127.0.0.1:1420/' });
  await until(() =>
    browser.evaluate(
      "!!document.querySelector('.featured-project') || !!document.querySelector('.editor-shell')",
    ),
  );
  await browser.evaluate(`(() => {
    const now = Date.now();
    const clips = Array.from({length: 1000}, (_, i) => ({id: 'bench-'+i, name: 'Clip '+i, kind: 'video', duration: 12, source: '/demo/coast.mp4', thumbnail: '/demo/coast.jpg', size: 0, addedAt: now, hasAudio: false}));
    const project = {id:'benchmark', name:'Performance fixture', description:'', createdAt:now, updatedAt:now, clips, timeline:clips.map((c,i)=>({id:'seg-'+i,clipId:c.id,in:0,out:6})),messages:[]};
    localStorage.setItem('cutline.projects.v1', JSON.stringify([project]));
    location.hash = 'project=benchmark';
  })()`);
  await browser.send('Page.reload');
  await delay(250);
  await until(() =>
    browser.evaluate(
      "!!document.querySelector('video') && document.querySelector('video').readyState >= 2",
    ),
  );
  await delay(500);
  await browser.send('Performance.enable');
  const before = await browser.send('Performance.getMetrics');
  const sample = await browser.evaluate(`(async () => {
    const video = document.querySelector('video');
    let time = 0;
    Object.defineProperty(video, 'currentTime', { configurable:true, get:()=>time });
    Object.defineProperty(video, 'seeking', { configurable:true, get:()=>false });
    const durations = [];
    for (let i=0; i<120; i++) {
      const start = performance.now();
      time = i / 24;
      video.dispatchEvent(new Event('timeupdate'));
      await new Promise(requestAnimationFrame);
      durations.push(performance.now()-start);
    }
    delete video.currentTime;
    delete video.seeking;
    durations.sort((a,b)=>a-b);
    return {ticks:durations.length,p50Ms:durations[60],p95Ms:durations[114],domNodes:document.querySelectorAll('*').length};
  })()`);
  const after = await browser.send('Performance.getMetrics');
  const metrics = (result) =>
    Object.fromEntries(result.metrics.map(({ name, value }) => [name, value]));
  const a = metrics(before),
    b = metrics(after);
  console.log(
    JSON.stringify(
      {
        fixture:
          '1000 clips / 1000 segments, 120 synthetic playhead updates (development build)',
        ...sample,
        scriptMs: (b.ScriptDuration - a.ScriptDuration) * 1000,
        taskMs: (b.TaskDuration - a.TaskDuration) * 1000,
        heapMiB: b.JSHeapUsedSize / 1024 / 1024,
      },
      null,
      2,
    ),
  );
} finally {
  browser.close();
}
