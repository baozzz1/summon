import{a as e,r as t,t as n}from"./i18n-YIZ94PuL.js";import{L as r,N as i,P as a,V as o,w as s}from"./router-DmWf1zNH.js";import{t as c}from"./live-D0VeMPic.js";import{d as l}from"./page-BvfONVkV.js";import{a as u}from"./field-row-CbIZQm7V.js";function d(e){let t=e.text.split(`
`).length,n=new Set((e.errors??[]).map(e=>e.line).filter(e=>typeof e==`number`)),i=[];for(let e=1;e<=t;e++)i.push(r`<span class=${n.has(e)?`err`:``}>${e}</span>${e<t?`
`:``}`);return r`<div class="yaml">
    <pre class="yaml-gutter" aria-hidden="true">${i}</pre>
    <textarea
      class="yaml-text"
      wrap="off"
      spellcheck="false"
      autocapitalize="off"
      autocomplete="off"
      rows=${t+1}
      aria-label=${e.label}
      ?readonly=${e.readonly}
      .value=${c(e.text)}
      @input=${t=>e.onInput(t.target.value)}
    ></textarea>
  </div>`}function f(n,i){if(!n.length&&!i.length)return a;let o=(n,i)=>r`<li class=${i}>
    ${s(i===`err`?`circleAlert`:`triangleAlert`,`icon-sm`)}
    <span
      >${n.line?r`<span class="tnum">${t(`dc4e3a9`,[n.line,n.col?e(`d3b067d`,[n.col]):``])}</span>：`:a}${l(n.message)}${n.suggestion?r` ${t(`dafcbfd`)} <code>${n.suggestion}</code>。`:a}</span
    >
  </li>`;return r`<ul class="issues-list" role="alert">
    ${n.map(e=>o(e,`err`))} ${i.map(e=>o(e,`warn`))}
  </ul>`}function p(e){let t=Math.max(8,e.text.split(`
`).length+1);return r`<textarea
    class="field-input doc-text"
    rows=${t}
    aria-label=${e.label}
    placeholder=${e.placeholder??``}
    ?readonly=${e.readonly}
    .value=${c(e.text)}
    @input=${t=>e.onInput(t.target.value)}
  ></textarea>`}function m(e){let t=new Map,n=(e,r)=>{let i=typeof e.element_id==`string`?e.element_id:null,a=i??r,o={};for(let[t,r]of Object.entries(e))o[t]=(t===`elements`||t===`columns`)&&Array.isArray(r)?r.flatMap(e=>{let t=n(e,a);return typeof e.element_id==`string`?[]:[t]}):r;return i&&t.set(i,{own:JSON.stringify(o),parent:r}),o};for(let r of e.messages)if(r.kind===`cot`){t.set(r.id,{own:JSON.stringify([r.kind,r.title,r.status]),parent:null});for(let e of r.items)t.set(e.id,{own:JSON.stringify(e),parent:r.id})}else if(r.kind===`card`){let e=r.card??{},i=(e.body?.elements??e.elements??[]).flatMap(e=>{let t=n(e,r.id);return typeof e.element_id==`string`?[]:[t]});t.set(r.id,{own:JSON.stringify([r.kind,i]),parent:null})}else t.set(r.id,{own:JSON.stringify(r),parent:null});return t}function h(e,t){let n=m(e),r=m(t),i=new Set;for(let[e,t]of n)r.get(e)?.own!==t.own&&i.add(e);for(let[e,t]of r){if(n.has(e))continue;let a=t.parent;for(;a&&!n.has(a);)a=r.get(a)?.parent??null;a&&i.add(a)}let a=e=>{let t=n.get(e)?.parent??null;for(;t;){if(i.has(t))return!0;t=n.get(t)?.parent??null}return!1};return[...i].filter(e=>!a(e))}function g(e,t){return h(t,e)}var _=class extends EventTarget{constructor(e,t,n,r=150){super(),this.call=e,this.scope=t,this.draft=n,this.delay=r,this.view=null,this.error=``,this.changed=new Set,this.focusKey=null,this.seq=0,this.quiet=!1,this.mark=e=>{let t=[];return this.focusKey&&this.regions(this.focusKey).includes(e)&&t.push(`is-region`),this.changed.has(e)&&t.push(`is-changed`),t.join(` `)}}emit(){this.dispatchEvent(new Event(`change`))}schedule(){this.timer&&clearTimeout(this.timer),this.timer=setTimeout(()=>{this.timer=void 0,this.request()},this.delay)}async request(){if(!this.scope)return;let e=++this.seq;try{let t=await this.call(`preview.render`,{scope:this.scope,draft:this.draft()});if(e!==this.seq)return;let n=this.view;this.view=t,this.error=``,n&&!this.quiet&&this.highlight(g(n,t)),this.quiet=!1,this.dispatchEvent(new Event(`view`))}catch(t){if(e!==this.seq)return;this.error=o(t)}this.emit()}highlight(e){this.flash&&clearTimeout(this.flash),this.changed=new Set(e),e.length&&(this.flash=setTimeout(()=>{this.changed=new Set,this.emit()},600))}setScope(e){this.scope&&JSON.stringify(this.scope)===JSON.stringify(e)||(this.scope=e,this.quiet=!0,this.request())}setFocus(e){e!==this.focusKey&&(this.focusKey=e,this.emit())}regions(e){return this.view?.regions[e]??[]}hasRegion(e){return this.regions(e).length>0}dispose(){this.timer&&clearTimeout(this.timer),this.flash&&clearTimeout(this.flash),this.seq++}},v={bash:`terminal`,read:`fileText`,write:`pencil`,search:`search`,doc:`fileText`,task:`circleCheck`,calendar:`clock`,meeting:`clock`};function y(e,t){switch(e.type){case`thinking`:return r`<div class="fs-cot-item thinking ${t(e.id)}" data-eid=${e.id}>${e.text}</div>`;case`text`:return r`<div class="fs-cot-item ${t(e.id)}" data-eid=${e.id}>${e.text}</div>`;case`tool`:return r`<div class="fs-cot-item ${t(e.id)}" data-eid=${e.id}>
        <div class="fs-tool">
          ${s(v[e.icon]??`terminal`)}<span class="fs-tool-name">${e.name}</span>${e.args?r`<span class="fs-tool-args">${e.args}</span>`:``}
        </div>
        ${e.result?r`<pre class="fs-tool-result">${e.result}</pre>`:``}
      </div>`}}function b(t,n,i,a=null){let o=a?t.items.slice(0,a.shown):t.items,c=o.length,l=a?e(`ddc9591`):t.status===`error`?e(`d01ad2b`):e(`df28461`),u=c>0&&!i.collapsed,d=a?`running`:t.status,f=a?s(`loaderCircle`,`icon-sm fs-spin`):s(t.status===`error`?`circleX`:`circleCheck`,`icon-sm`);return r`<div class="fs-cot ${d} ${n(t.id)}" data-eid=${t.id} ?data-open=${u}>
    <button type="button" class="fs-cot-head" aria-expanded=${u?`true`:`false`} ?disabled=${!c} @click=${i.toggle}>
      ${f}
      <span class="fs-cot-title ${a?`fs-shimmer`:``}">${t.title}</span>
      <span class="fs-cot-steps">${c?e(`d310f38`,[l,c]):l}</span>
      ${c?s(`chevronDown`,`icon-sm fs-cot-chevron`):``}
    </button>
    ${u?r`<div class="fs-cot-body">${o.map(e=>y(e,n))}</div>`:``}
  </div>`}var x=new Set([`grey`,`gray`,`red`,`green`,`orange`,`blue`]);function S(e,t){let n=e.createElement(`span`);return n.className=t,n}var C=/^<font\s+color\s*=\s*['"]?([a-zA-Z-]+)['"]?\s*>/,w=/^<at\s+(?:id|user_id|open_id|email)\s*=\s*['"]?([^'">\s]*)['"]?\s*>([^<]*)<\/at>/,T=/^\[([^\]\n]+)\]\(([^)\s]+)\)/;function E(e,t,n){let r=e.indexOf(`
`,t),i=r<0?e.length:r,a=e.indexOf(n,t);return a>=0&&a<i?a:-1}function D(t,n,r=document){let i=``,a=()=>{i&&n.appendChild(r.createTextNode(i)),i=``},o=0;for(;o<t.length;){let s=t.slice(o),c=t[o];if(c==="`"){let e=E(t,o+1,"`");if(e>o+1){a();let i=S(r,`fs-code`);i.textContent=t.slice(o+1,e),n.appendChild(i),o=e+1;continue}}if(s.startsWith(`**`)||s.startsWith(`~~`)){let e=s.slice(0,2),i=E(t,o+2,e);if(i>o+2){a();let s=r.createElement(e===`**`?`strong`:`s`);D(t.slice(o+2,i),s,r),n.appendChild(s),o=i+2;continue}}if(c===`*`&&!/[A-Za-z0-9_/.*\\]/.test(t[o-1]??``)&&t[o+1]&&!/[\s*]/.test(t[o+1])){let e=E(t,o+1,`*`);if(e>o+1&&!/\s/.test(t[e-1])&&!/[A-Za-z0-9_*]/.test(t[e+1]??``)){a();let i=r.createElement(`em`);D(t.slice(o+1,e),i,r),n.appendChild(i),o=e+1;continue}}if(c===`[`){let e=T.exec(s);if(e){a();let t=S(r,`fs-link`);D(e[1],t,r),n.appendChild(t),o+=e[0].length;continue}}if(c===`<`){let i=C.exec(s);if(i){let e=t.indexOf(`</font>`,o+i[0].length);if(e>=0){a();let s=i[1].toLowerCase(),c=S(r,x.has(s)?`fs-c-${s===`gray`?`grey`:s}`:``);D(t.slice(o+i[0].length,e),c,r),n.appendChild(c),o=e+7;continue}}let c=w.exec(s);if(c){a();let t=S(r,`fs-at`),i=c[1]??``;t.textContent=`@${(c[2]??``).trim()||(i===`all`?e(`d06d9ae`):i||e(`d61edb9`))}`,n.appendChild(t),o+=c[0].length;continue}}i+=c,o++}a()}function O(e,t=document){let n=t.createDocumentFragment(),r=e.replace(/\r\n?/g,`
`).split(`
`),i=[],a=null,o=()=>{if(!i.length)return;let e=t.createElement(`p`);i.forEach((n,r)=>{r&&e.appendChild(t.createElement(`br`)),D(n,e,t)}),n.appendChild(e),i=[]},s=()=>{a=null};for(let e=0;e<r.length;e++){let c=r[e];if(c.trimStart().startsWith("```")){o(),s();let i=[];for(e++;e<r.length&&!r[e].trimStart().startsWith("```");)i.push(r[e++]);let a=t.createElement(`pre`);a.textContent=i.join(`
`),n.appendChild(a);continue}let l=/^\s*[-*+]\s+(.*)$/.exec(c),u=/^\s*\d+[.)]\s+(.*)$/.exec(c);if(l||u){o();let e=!l;(!a||a.ordered!==e)&&(a={el:t.createElement(e?`ol`:`ul`),ordered:e},n.appendChild(a.el));let r=t.createElement(`li`);D((l??u)[1],r,t),a.el.appendChild(r)}else c.trim()?(s(),i.push(c)):(o(),s())}return o(),n}var k=()=>``;function A(e){return typeof e==`string`?e:e&&typeof e==`object`&&typeof e.content==`string`?e.content:``}function j(e,t){let n=typeof e.element_id==`string`?e.element_id:``;return{eid:n||a,cls:n?t(n):``}}function M(e){let t=typeof e==`string`?{small:4,medium:8,large:12,extra_large:16}[e]??Number.parseInt(e,10):8;return`gap-${Number.isFinite(t)?[0,4,8,12,16].reduce((e,n)=>Math.abs(n-t)<Math.abs(e-t)?n:e):8}`}var N=(e,t)=>Array.isArray(e)?e.map(e=>F(e,t)):a,P={markdown:(e,t)=>{let{eid:n,cls:i}=j(e,t),a=e.text_size===`notation`?`notation`:``,o=e.text_align===`center`?`align-center`:e.text_align===`right`?`align-right`:``;return r`<div class="fsc-md ${a} ${o} ${i}" data-eid=${n}>${O(A(e.content))}</div>`},hr:(e,t)=>{let{eid:n,cls:i}=j(e,t);return r`<div class="fsc-hr ${i}" data-eid=${n} role="separator"></div>`},img:(t,n)=>{let{eid:i,cls:a}=j(t,n),o=A(t.alt)||e(`dd24c10`);return r`<div class="fsc-img ${a}" data-eid=${i}>${s(`fileText`,`icon-sm`)}<span>${o}</span></div>`},column_set:(e,t)=>{let{eid:n,cls:i}=j(e,t),a=typeof e.flex_mode==`string`?e.flex_mode:`none`;return r`<div class="fsc-cols ${a} ${M(e.horizontal_spacing)} ${i}" data-eid=${n}>${N(e.columns,t)}</div>`},column:(e,t)=>{let{eid:n,cls:i}=j(e,t),a=e.width===`weighted`?`weighted`:`auto`,o=e.vertical_align===`center`?`v-center`:e.vertical_align===`bottom`?`v-bottom`:``;return r`<div class="fsc-col ${a} ${o} ${i}" data-eid=${n}>${N(e.elements,t)}</div>`},button:(e,t)=>{let{eid:n,cls:i}=j(e,t),a=typeof e.type==`string`?e.type:`default`;return r`<span class="fsc-btn ${a} ${e.disabled?`disabled`:``} ${i}" data-eid=${n} aria-hidden="true"
      >${A(e.text)}</span
    >`},collapsible_panel:(e,t)=>{let{eid:n,cls:i}=j(e,t),a=A((e.header??{}).title);return r`<div class="fsc-panel ${e.expanded?`expanded`:``} ${i}" data-eid=${n}>
      <div class="fsc-panel-head">${s(`chevronRight`,`icon-sm`)}<span>${O(a)}</span></div>
      <div class="fsc-panel-body">${N(e.elements,t)}</div>
    </div>`}};function F(e,n=k){let i=typeof e?.tag==`string`?e.tag:``,a=Object.hasOwn(P,i)?P[i]:void 0;if(!a){let{eid:a,cls:o}=j(e??{},n);return r`<div class="fsc-unknown ${o}" data-eid=${a}>${t(`d04038f`)} <code>&lt;${i||`?`}&gt;</code></div>`}return a(e,n)}function I(e,t=k){let n=e??{},i=n.body?.elements??n.elements;return r`<div class="fsc">${N(i,t)}</div>`}var L=650;function R(e){return[...e.trim()][0]??`?`}function z(e){let t=document.createDocumentFragment();return D(e,t),t}function B(e){let t=[],n=/@[^\s@]+/g,i=0;for(let a of e.matchAll(n))a.index>i&&t.push(e.slice(i,a.index)),t.push(r`<span class="fs-at">${a[0]}</span>`),i=a.index+a[0].length;return i<e.length&&t.push(e.slice(i)),t}var V=class extends i{static{this.properties={ctl:{attribute:!1},botName:{attribute:!1}}}constructor(){super(),this.locale=new n(this),this.onCtl=()=>this.requestUpdate(),this.bound=null,this.revealed=null,this.cotFold={collapsed:!1,toggle:()=>{this.cotFold.collapsed=!this.cotFold.collapsed,this.requestUpdate()}},this.live=null,this.played=!1,this.shown=typeof IntersectionObserver>`u`,this.observer=null,this.ctl=null,this.botName=`agent`}createRenderRoot(){return this}replay(){clearTimeout(this.liveTimer);let e=this.ctl?.view?.messages.find(e=>e.kind===`cot`);if(!e||e.kind!==`cot`)return;let t=e.items.length;this.live={shown:0};let n=()=>{this.live&&(this.live.shown<t?(this.live={shown:this.live.shown+1},this.liveTimer=setTimeout(n,L)):this.live=null,this.requestUpdate())};this.liveTimer=setTimeout(n,L),this.requestUpdate()}willUpdate(e){e.has(`ctl`)&&this.bound!==this.ctl&&(this.bound?.removeEventListener(`change`,this.onCtl),this.ctl?.addEventListener(`change`,this.onCtl),this.bound=this.ctl),this.autoplay()}autoplay(){!this.played&&this.shown&&this.ctl?.view&&(this.played=!0,window.matchMedia(`(prefers-reduced-motion: reduce)`).matches||this.replay())}firstUpdated(){if(typeof IntersectionObserver>`u`)return;this.observer=new IntersectionObserver(e=>{e.some(e=>e.isIntersecting)&&(this.shown=!0,this.observer?.disconnect(),this.observer=null,this.autoplay())});let e=this.querySelector(`.pv-scroll`);e&&this.observer.observe(e)}disconnectedCallback(){super.disconnectedCallback(),this.bound?.removeEventListener(`change`,this.onCtl),this.bound=null,clearTimeout(this.liveTimer),this.live=null,this.observer?.disconnect(),this.observer=null}updated(){let e=this.ctl?.changed;if(!e?.size||e===this.revealed)return;this.revealed=e;let t=this.querySelector(`.pv-scroll`),n=t?.querySelector(`.is-changed`);if(!t||!n)return;let r=t.getBoundingClientRect(),i=n.getBoundingClientRect();if(i.top>=r.top&&i.bottom<=r.bottom)return;let a=!window.matchMedia(`(prefers-reduced-motion: reduce)`).matches;t.scrollBy({top:i.top-r.top-12,behavior:a?`smooth`:`auto`})}renderMessages(e,n){let i=[],o=[],s=()=>{o.length&&(i.push(r`<div class="fs-msg">
        <span class="fs-avatar bot" aria-hidden="true">${R(this.botName)}</span>
        <div class="fs-msg-main">
          <div class="fs-name">${this.botName}<span class="fs-bot-tag">${t(`da4d144`)}</span><time>14:32</time></div>
          ${o}
        </div>
      </div>`),o=[])};for(let t of e)switch(t.kind){case`user`:s(),i.push(r`<div class="fs-msg">
            <span class="fs-avatar" aria-hidden="true">${R(t.sender)}</span>
            <div class="fs-msg-main ${n(t.id)}" data-eid=${t.id}>
              <div class="fs-name">${t.sender}<time>14:31</time></div>
              <div class="fs-bubble">${B(t.text)}</div>
              ${t.reactions.length?r`<div class="fs-reactions">
                    ${t.reactions.map(e=>r`<span class="fs-reaction" title=${e}>${u(e,e)}${this.botName}</span>`)}
                  </div>`:a}
            </div>
          </div>`);break;case`cot`:o.push(b(t,n,this.cotFold,this.live));break;case`card`:if(this.live)break;o.push(r`<div class="fs-card-wrap ${n(t.id)}" data-eid=${t.id}>${I(t.card,n)}</div>`)}return s(),i}renderFeed(e,n){let i=e.find(e=>e.kind===`feed`);return!i||this.live?a:r`<div class="fs-feed-label">${t(`dc980de`)}</div>
      <div class="fs-feed ${n(i.id)}" data-eid=${i.id}>
        <span class="fs-avatar group" aria-hidden="true">${R(i.chat_name)}</span>
        <div>
          <div class="fs-feed-top"><strong>${i.chat_name}</strong><time>14:32</time></div>
          <div class="fs-feed-text">${z(i.text)}</div>
        </div>
      </div>`}render(){let n=this.ctl,i=n?.view,a=n?.mark??(()=>``);return r`<section class="pv" aria-label="${e(`d23d081`)}">
      <header class="pv-head">
        <div class="pv-head-text">
          <h2>${t(`dae9f6a`)}</h2>
          <p>${i?.title??e(`ded25f6`)}<span class="pv-note"> ${t(`decf66c`)}</span></p>
        </div>
        <button class="icon-btn" title="${e(`ddc4db9`)}" aria-label="${e(`ddc4db9`)}" ?disabled=${!i} @click=${()=>this.replay()}>
          ${s(`rotateCcw`,`icon-sm`)}
        </button>
      </header>
      <div class="pv-scroll">
        ${i?r`<div class="fs">
              <div class="fs-chat">${this.renderMessages(i.messages,a)}</div>
              ${this.renderFeed(i.messages,a)}
            </div>`:n?.error?r`<p class="pv-status">${t(`de042e1`,[n.error])}</p>`:r`<div class="sk-row"><span class="sk"></span><span class="sk"></span></div>`}
      </div>
      <p class="pv-foot">${t(`d857433`)}</p>
    </section>`}};customElements.define(`sm-preview`,V);export{d as i,p as n,f as r,_ as t};