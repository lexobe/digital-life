import init from '/console/assets/survive_console.js';
init().catch(() => {
  const root = document.getElementById('app');
  root.textContent = '观测台加载失败。请刷新页面，或检查服务器前端资源是否完整。';
});
