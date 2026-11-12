import './styles/tokens.css';
import './styles/base.css';
import './styles/layout.css';
import './styles/components.css';
import './styles/preview.css';
import { fetchTransport, type Transport } from './api/client';
import './app/shell';
import { store } from './app/store';

async function transport(): Promise<Transport> {
  // 示例环境只在开发与截图构建里存在;生产构建里这个分支连同 mock 模块一起被摇掉。
  if (__SUMMON_MOCK__) {
    const { startMock } = await import('./api/mock-bar');
    return startMock();
  }
  return fetchTransport;
}

void transport().then((t) => {
  store.init(t);
  return store.boot();
});
