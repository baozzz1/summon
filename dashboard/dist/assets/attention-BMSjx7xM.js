import{a as e}from"./i18n-YIZ94PuL.js";import{L as t,P as n,V as r,m as i}from"./router-DmWf1zNH.js";var a=class{constructor(){this.data=null,this.error=``,this.pending=null}refresh(e){return this.pending||=e().then(e=>{this.data=e,this.error=``},e=>{this.error=r(e)}).finally(()=>{this.pending=null}),this.pending}};function o(e){return/^lark:\/\/(applink\.feishu\.cn|applink\.larksuite\.com)\/client\/chat\/open\?openChatId=[A-Za-z0-9_%.-]+(?:&position=[0-9]+)?$/.test(e)?e:null}function s(r,a=``,s=!1){let c=r.data,l=(c?.items??[]).filter(e=>!a||e.agent===a);return t`<section class="page-section" aria-labelledby="attention-title">
    <div class="group-head"><h2 id="attention-title">${e(`d485504`)} ${c?l.length:``}</h2></div>
    <p class="muted">${e(`d06ab6f`)}</p>
    ${r.error?t`<p role="alert">${e(`d4e22d1`)}: ${r.error}</p>`:n}
    ${c&&r.error?t`<p role="status">${e(`d694821`,[i(c.observed_at)])}</p>`:n}
    ${c?.partial?t`<p role="status">${e(`da616f2`)}</p>`:n}
    ${c?s?t`<a class="link-btn" href="/sessions">${e(`d04d1e1`)}</a>`:l.length?t`<div class="card">${l.map(a=>{let s=!r.error&&o(a.link),l=a.source===`claude_permission`?e(`d405539`):a.source===`codex_approval`?e(`d685a43`):e(`de0ccb9`);return t`<div class="list-row"><div class="list-main">
            <div class="list-title">${a.agent_display||a.agent} · ${a.chat_name}</div>
            <div class="list-sub">${l}${a.summary?t` · ${a.summary}`:n}</div>
            <div class="list-sub">${a.question_count?e(`d372384`,[a.waited_secs,a.question_count]):e(`d66e285`,[a.waited_secs])}</div>
            ${s?t`<a class="link-btn" href=${s} rel="noopener noreferrer">${a.link_target===`chat`?e(`d0a7d4a`):e(`d4352b8`)}</a>`:t`<span class="muted">${r.error?e(`d694821`,[i(c.observed_at)]):e(`da5f172`)}</span>`}
          </div></div>`})}</div>`:t`<p>${r.error||c.partial?e(`d4e22d1`):e(`d341591`)}</p>`:t`<p>${r.error?e(`d4e22d1`):e(`d2b13f0`)}</p>`}
    </section>`}export{s as n,a as t};