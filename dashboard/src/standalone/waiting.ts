// 远程链接的落地页:GET 不改任何状态(飞书与安全扫描会预取链接)。表单 POST 到 daemon 发页时填好的
// /l/<链接>/enter:签发人此刻仍有权限就用掉链接、发会话 cookie 并 303 到 /。有 JS 时直接替人点一下:
// 打开即进,与文件预览的打开链接一致;代价是会执行脚本的链接扫描器也会替人用掉链接(spec「已知限制」)。
// 失败的说明页由服务端给出。
// 自带一份 token 与基础样式(@import 内联),不与控制台本体共用样式块,保证加载顺序。
import './standalone.css';

const form = document.getElementById('enter') as HTMLFormElement | null;
const button = document.getElementById('enter-btn') as HTMLButtonElement | null;

form?.addEventListener('submit', () => {
  // 链接只能用一次:提交后按钮置灰,免得连点两次第二次落到「已失效」。
  if (button) button.disabled = true;
});

if (form && /^\/l\/[0-9a-f]{32}\/?$/.test(location.pathname)) {
  form.requestSubmit();
}
