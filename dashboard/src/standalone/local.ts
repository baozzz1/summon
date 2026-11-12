import { t as translate } from '../i18n';
// 本机码落地页:表单 POST 回同一地址消耗本机码(服务端只认本机地址、一次性、60 秒内),
// 成功后 303 跳到 /。有 JS 时直接替人点一下;失败的说明页由服务端给出。
// 自带一份 token 与基础样式(@import 内联),不与控制台本体共用样式块,保证加载顺序。
import './standalone.css';

const form = document.getElementById('enter') as HTMLFormElement | null;
const button = document.getElementById('enter-btn') as HTMLButtonElement | null;
const status = document.getElementById('status');

form?.addEventListener('submit', () => {
  // 码只能用一次:提交后按钮置灰,免得连点两次第二次落到「已用过」。
  if (button) button.disabled = true;
  if (status) status.textContent = translate('d8f0c12');
});

if (form && /^\/local\/[A-Za-z0-9_-]{8,256}\/?$/.test(location.pathname)) {
  form.requestSubmit();
}
