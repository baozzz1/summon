import{a as e,r as t}from"./i18n-YIZ94PuL.js";import{L as n,P as r,T as i,V as a,_ as o,a as s,c,t as l,w as u}from"./router-DmWf1zNH.js";import{i as d,n as f,o as p,r as m,t as h}from"./labels-DeJm4sRU.js";import{a as g,c as _,f as v,i as y,m as b,n as x,o as S,p as C,r as w,t as T,u as E}from"./page-BvfONVkV.js";import{t as D}from"./ident-Bw3GTApD.js";import{o as O}from"./controls-DMP3WW_j.js";import{i as k,n as A,r as j,t as M}from"./panel-oHAd-YCH.js";import{i as N,n as P,r as F}from"./field-row-CbIZQm7V.js";var I=class{constructor(e,t,n,r){this.alias=e,this.call=t,this.update=n,this.finished=r,this.job=null,this.busy=!1,this.saved=!1,this.name=``,this.error=``,this.progress=``,this.disposed=!1}dispose(){this.disposed=!0}async poll(t){for(this.job=t,this.update();t.status===`preparing`||t.status===`cleaning`;){if(await new Promise(e=>setTimeout(e,800)),this.disposed)throw Error(e(`d754e37`));t=await this.call(`agents.cleanup.status`,{job:t.job}),this.job=t,this.update()}if(t.status===`failed`)throw Error(t.message||e(`d5fc5fc`));return t}async run(e){if(!this.busy){this.busy=!0,this.error=``,this.update();try{await e()}catch(e){this.error=a(e)}finally{this.busy=!1,this.progress=``,this.disposed||this.update()}}}prepare(){return this.run(async()=>{this.saved=!1,this.name=``,this.job=null,await this.poll(await this.call(`agents.cleanup.prepare`,{agent:this.alias}))})}download(){return this.run(async()=>{let t=this.job;if(!t||t.status!==`ready`||t.bytes===null)return;this.saved=!1;let n=[],r=0;for(;r<t.bytes;){if(this.disposed)return;let i=await this.call(`agents.cleanup.download`,{job:t.job,offset:r}),a=Uint8Array.from(atob(i.base64),e=>e.charCodeAt(0));if(i.offset!==r||i.next!==r+a.length||i.next>t.bytes||a.length===0||i.done!==(i.next===t.bytes))throw Error(e(`de40b7d`));n.push(a),r=i.next,this.progress=e(`d4a36c2`,[Math.floor(r/t.bytes*100)]),this.update()}let i=new Blob(n,{type:`application/gzip`});if(globalThis.crypto?.subtle&&t.sha256){let n=await crypto.subtle.digest(`SHA-256`,await i.arrayBuffer());if(Array.from(new Uint8Array(n),e=>e.toString(16).padStart(2,`0`)).join(``)!==t.sha256)throw Error(e(`da08696`))}let a=URL.createObjectURL(i),o=document.createElement(`a`);o.href=a,o.download=t.filename,o.click(),setTimeout(()=>URL.revokeObjectURL(a),6e4),this.job=await this.call(`agents.cleanup.status`,{job:t.job})})}async confirm(){if(this.job?.downloaded&&this.saved&&this.name===this.alias&&!this.busy&&await _({title:e(`d57f820`,[this.alias]),body:e(`d41de15`),confirm:e(`d89c90f`),danger:!0}))return this.run(async()=>{let e=await this.poll(await this.call(`agents.cleanup.confirm`,{job:this.job.job,saved:!0,agent:this.alias}));e.status===`done`&&(e.message?this.error=e.message:await this.finished())})}render(){let i=this.job;return n`<section class="group" aria-label="${e(`d6187b8`)}">
      <header class="group-head"><div><h2>${t(`d2a53b2`)}</h2><p>${t(`d80e385`)}</p></div></header>
      <div class="card card-pad">
        <p>${t(`d7ec841`)}</p>
        <button class="btn" ?disabled=${this.busy} @click=${()=>this.prepare()}>${e(i?`d45ef18`:`d1099b9`)}</button>
        ${this.busy?n`<p role="status">${this.progress||(i?.status===`cleaning`?e(`d6dfddc`):e(`d574ec7`))}</p>`:r}
        ${i?.status===`ready`?n`<p>${i.filename} · ${((i.bytes??0)/1024/1024).toFixed(1)} MB</p>
          <button class="btn btn-primary" ?disabled=${this.busy} @click=${()=>this.download()}>${t(`d3d1ec6`)}</button>
          ${i.downloaded?n`<p><label><input type="checkbox" .checked=${this.saved} @change=${e=>{this.saved=e.target.checked,this.update()}}> ${t(`d55060d`)}</label></p>
            <label>${t(`d88c7bb`)} <code>${this.alias}</code> ${t(`d7ba2d6`)}
              <input class="input" aria-label="${e(`dada317`)}" .value=${this.name} @input=${e=>{this.name=e.target.value,this.update()}}>
            </label>
            <p><button class="btn btn-danger" ?disabled=${this.busy||!this.saved||this.name!==this.alias} @click=${()=>this.confirm()}>${t(`d89c90f`)}</button></p>`:r}`:r}
        ${this.error?n`<p class="row-error" role="alert">${this.error}</p>`:r}
      </div>
    </section>`}},L={overview:`identity`,runtime:`runtime`,permissions:`permissions`,messaging:`messaging`},R=class extends T{constructor(...e){super(...e),this.alias=``,this.tab=`overview`,this.target=null,this.fields=new Map,this.detail=null,this.chats=null,this.toggling=!1,this.cleanup=null}setup(t){this.cleanup?.dispose(),this.cleanup=new I(t,this.call,()=>this.requestUpdate(),async()=>{S(e(`d9ee71e`,[t]),`ok`),await i.refreshStatus(!0),c(s({page:`agents`}))}),this.alias=t;let n={level:`agent`,agent:t};this.config=new v(n,()=>e(`db77c1d`,[t]),()=>this.fields,this.call,()=>{this.preview.schedule(),this.requestUpdate()}),this.claude=new C({level:`agent`,agent:t,file:`CLAUDE.md`},`CLAUDE.md`,this.call,()=>this.requestUpdate()),this.soul=new C({level:`agent`,agent:t,file:`SOUL.md`},`SOUL.md`,this.call,()=>this.requestUpdate()),this.raw=new b({file:`agent`,agent:t},`${t}/agent.yaml`,this.call,E,()=>this.requestUpdate()),this.preview?.dispose(),this.preview=new M(this.call,n,()=>this.config.changeList()),this.bindPreview(this.preview),this.detail=null,this.chats=null,this.loading=!0}registerEdits(){this.edits.replace(this.tab===`yaml`?[this.raw]:[this.config,this.claude,this.soul])}willUpdate(){let e=this.route;if(e){if(e.alias!==this.alias)this.setup(e.alias),this.tab=e.tab,this.registerEdits(),this.refresh();else if(e.tab!==this.tab){let t=this.tab===`yaml`;this.tab=e.tab,t!==(e.tab===`yaml`)&&(this.registerEdits(),e.tab===`yaml`?this.raw.load():this.config.load()),e.tab===`messaging`&&this.preview.request()}e.key&&e.key!==this.target&&(this.target=e.key,this.focusTarget(e.key))}}languageChanged(){this.fields=new Map((i.state.schema?.fields??[]).map(e=>[e.key,e])),this.preview&&this.preview.request(),super.languageChanged()}connectedCallback(){super.connectedCallback();for(let e of i.state.schema?.fields??[])this.fields.set(e.key,e)}disconnectedCallback(){super.disconnectedCallback(),this.preview?.dispose(),this.cleanup?.dispose()}watchKeys(){return[`chat:*`]}sameContext(e){return e.page!==`agent`||e.alias!==this.alias?!1:e.tab===`yaml`==(this.tab===`yaml`)}async refresh(){let e=this.alias;try{let[t,n]=await Promise.all([this.call(`agents.get`,{agent:e}),this.call(`chats.list`,{})]);if(e!==this.alias)return;this.detail=t,this.chats=n.filter(t=>t.agents.some(t=>t.agent===e)),this.loadError=``;let r=[this.claude.load(),this.soul.load()];t.summary.load_error||r.push(this.config.load()),(this.tab===`yaml`||t.summary.load_error)&&r.push(this.raw.load()),await Promise.all(r),t.summary.load_error&&this.raw.view&&await this.raw.check(),this.tab===`messaging`&&this.preview.request()}catch(e){this.loadError=a(e)}this.loading=!1,this.requestUpdate()}env(){let e=this.detail,t={draft:this.config,level:`agent`,preview:this.tab===`messaging`?this.preview:null,target:this.highlight};return e&&(t.members=e.members,t.suggestions={model:e.model_candidates},e.effort_options.length&&(t.options={effort:e.effort_options.map(e=>({value:e,label:e}))})),t}sectionGroups(t){let r=i.state.schema;if(this.detail?.summary.load_error)return this.brokenNotice();if(this.config.loadError)return w(e(`de5ed04`),this.config.loadError,()=>this.refresh());if(!this.config.view)return g(5);let a=this.env();return n`${N(r,t).map(e=>F(e,P(r,e.id),a))}`}brokenNotice(){let e=this.detail;return n`<div class="banner banner-error" role="alert">
      ${u(`circleAlert`)}
      <div class="banner-body">
        <strong>${t(`dce82d1`)}</strong>
        <p class="mono">${e.summary.load_error}</p>
        <p>${t(`d78ddf8`)}</p>
        <div class="banner-actions">
          <a class="btn btn-sm" href=${s({page:`agent`,alias:this.alias,tab:`yaml`})}>${t(`d13fa63`)}</a>
        </div>
      </div>
    </div>`}async toggleEnabled(t){let n=this.detail;if(n&&(t||await _({title:e(`d4fd748`,[n.summary.display||this.alias]),body:e(`d1cae81`),confirm:e(`d4e6fd0`),danger:!0}))){this.toggling=!0,this.requestUpdate();try{let r=await this.call(`agents.set_enabled`,{agent:this.alias,enabled:t});this.detail={...n,summary:r},S(e(t?`dc17bd5`:`d1e8856`,[r.display||this.alias]),`ok`),await i.refreshStatus(!0)}catch(t){S(e(`d860480`,[a(t)]),`error`)}this.toggling=!1,this.requestUpdate()}}tabOverview(){let i=this.detail;if(!i)return g(4);let a=i.summary;return n`${this.sectionGroups(`identity`)}
      <section class="group" aria-labelledby="gh-enable">
        <header class="group-head"><h2 id="gh-enable">${t(`df4f0ea`)}</h2></header>
        <div class="card">
          <div class="row row--inline ${i.can_toggle_enabled?``:`is-disabled`}">
            <div class="row-grid">
              <div class="row-head">
                <div class="row-title"><span class="row-label">${t(`d95f570`)}</span></div>
                <p class="row-desc">${t(`dfe3fc8`)}</p>
              </div>
              <div class="row-ctl" title=${i.deny_reason??r}>
                ${O({checked:a.enabled,label:e(`d95f570`),disabled:!i.can_toggle_enabled||this.toggling||this.cleanup?.busy,onChange:e=>this.toggleEnabled(e)})}
                ${i.deny_reason?n`<span class="row-lock">${u(`lock`,`icon-sm`)}<span>${i.deny_reason}</span></span>`:r}
              </div>
            </div>
          </div>
        </div>
      </section>
      ${!a.enabled&&i.can_toggle_enabled?this.cleanup?.render():r}
      <section class="group" aria-labelledby="gh-feishu">
        <header class="group-head">
          <div>
            <h2 id="gh-feishu">${t(`da0def6`)}</h2>
            <p>${t(`d30d119`)} <code>summon bind ${this.alias}</code>。</p>
          </div>
        </header>
        <div class="card card-pad">
          <dl class="kv-list">
            <dt>${t(`db144ad`)}</dt>
            <dd class="mono">${i.app_id_env}</dd>
            <dt>${t(`d469e5d`)}</dt>
            <dd>${i.scope===`thread`?e(`d3b8362`):i.scope===`chat`?e(`dcc886f`):i.scope}</dd>
            <dt>${t(`d2dfe17`)}</dt>
            <dd>
              ${a.credentials===`ready`?n`<span class="chip chip-ok">${t(`dab27f8`)}</span>`:n`<span class="chip chip-danger">${t(`d65364e`)}</span> <span class="muted">${t(`db56b0b`)}</span>`}
            </dd>
          </dl>
        </div>
      </section>`}tabPermissions(){let r=this.chats??[];return n`${this.sectionGroups(`permissions`)}
      <section class="group" aria-labelledby="gh-chatops">
        <header class="group-head">
          <div>
            <h2 id="gh-chatops">${t(`de3137d`)}</h2>
            <p>${t(`d49cff2`,[this.detail?.summary.display||this.alias])}</p>
          </div>
        </header>
        ${r.length?n`<div class="card">${r.filter(e=>e.kind===`group`).map(e=>this.chatLink(e,`permissions`))}</div>`:n`<div class="card">${x(e(`d437c79`),e(`d9b02e4`))}</div>`}
      </section>`}chatLink(i,a){let c=i.agents.find(e=>e.agent===this.alias);return n`<a class="list-row" href=${s({page:`chat`,chatId:i.chat_id,agent:this.alias})}>
      <span class="avatar" aria-hidden="true">${m(i)}</span>
      <div class="list-main">
        <div class="list-title">
          <span>${p(i)||i.kind===`p2p`?d(i):D(i.chat_id,{agent:this.alias})}</span>${c?.live?n`<span class="chip chip-ok">${t(`d7d817d`)}</span>`:r}
        </div>
        ${p(i)?n`<div class="list-sub mono">${i.chat_id}</div>`:r}
      </div>
      <div class="list-aside">
        ${a===`chats`?n`<span class="count">${c?.overrides?e(`ded467e`,[c.overrides]):e(`d27e364`)}</span>
              <span class="hide-narrow">${o(c?.last_active)}</span>`:n`<span>${t(`d051836`)}</span>`}
        ${u(`chevronRight`,`icon-sm`)}
      </div>
    </a>`}tabChats(){return this.chats?n`<div class="group-head">
        <div>
          <h2>${t(`d295aa9`)}</h2>
          <p>${t(`dcf895d`,[this.detail?.summary.display||this.alias])}</p>
        </div>
      </div>
      ${this.chats.length?n`<div class="card">${this.chats.map(e=>this.chatLink(e,`chats`))}</div>`:n`<div class="card">${x(e(`d437c79`),e(`d797cab`))}</div>`}`:g(3)}docCard(t,i,a){return n`<section class="card card-pad doc-card" aria-label=${i}>
      <div class="editor-head">
        <div>
          <h2>${i}</h2>
          <p class="muted">${a}</p>
        </div>
        ${t.view?n`<span class="editor-path">${t.view.path}</span>`:r}
      </div>
      ${t.loadError?n`<p class="row-error">${t.loadError}</p>`:t.view?n`${A({text:t.text,label:i,readonly:!t.view.editable,placeholder:t.view.exists?``:e(`d9b7cea`),onInput:e=>t.setText(e)})}${t.error?n`<p class="row-error">${u(`circleAlert`,`icon-sm`)}${t.error}</p>`:r}`:n`<span class="sk sk-w1"></span>`}
    </section>`}tabDocs(){return n`${this.docCard(this.claude,`CLAUDE.md`,e(`d04ed47`))}
    ${this.docCard(this.soul,`SOUL.md`,e(`daf99f4`))}`}tabYaml(){let i=this.raw,a=this.detail?.summary.load_error;if(i.loadError)return w(e(`dd523c9`),i.loadError,()=>i.load());if(!i.view)return g(8);let o=i.lastCheck;return n`${a?n`<div class="banner banner-error" role="alert">
            ${u(`circleAlert`)}
            <div class="banner-body">
              <strong>${t(`dc58a0c`)}</strong>
              <p class="mono">${a}</p>
              <p>${t(`df21f72`)}</p>
            </div>
          </div>`:r}
      <div class="editor-head">
        <div>
          <span class="editor-path">${i.view.path}</span>
          <p class="muted">${t(`db0d77c`)}<code>cognition:</code> ${t(`d1b9a28`)}</p>
        </div>
        <button class="btn btn-sm" @click=${()=>i.check()}>${t(`de12bee`,[u(`circleCheck`,`icon-sm`)])}</button>
      </div>
      ${o&&!o.errors.length&&!o.warnings.length?n`<p class="row-note">${u(`check`,`icon-sm`)}<span>${t(`d6260cd`)}</span></p>`:r}
      ${o?j(o.errors,o.warnings):r}
      ${i.error?n`<p class="row-error">${u(`circleAlert`,`icon-sm`)}${i.error}</p>`:r}
      ${k({text:i.text,label:e(`dc856e6`),errors:o?.errors??[],readonly:!i.view.editable,onInput:e=>i.setText(e)})}`}header(){let t=i.state.status?.agents??[],a=this.detail?.summary??t.find(e=>e.alias===this.alias),o=i.state.status?.restart_pending.map(e=>e.file)??[],l=a?f(a,o):null,d=n`<span class="select-wrap agent-pick">
      <select
        class="select"
        aria-label="${e(`d15ca36`)}"
        .value=${this.alias}
        @change=${e=>{let t=e.target.value;c(s({page:`agent`,alias:t,tab:this.tab})).then(()=>{e.target.value=this.alias})}}
      >
        ${t.map(e=>n`<option value=${e.alias} ?selected=${e.alias===this.alias}>${e.display||e.alias} · ${e.alias}</option>`)}
      </select>
      ${u(`chevronDown`)}
    </span>`;return y(n`<span class="title-with-pick">${a?.display||this.alias}${l?n`<span class="chip ${l.tone?`chip-${l.tone}`:`chip-quiet`}">${l.text}</span>`:r}</span>`,n`<span class="mono">${this.alias}</span>${a&&!a.load_error?n` · ${h(a)}`:r}`,d,{href:`/agents`,label:`Agents`})}tabs(){return n`<nav class="tabs" aria-label="${e(`d720ab3`)}">
      ${l.map(e=>n`<a class="tab" href=${s({page:`agent`,alias:this.alias,tab:e.id})} aria-current=${e.id===this.tab?`page`:r}
          >${e.label}</a
        >`)}
    </nav>`}body(){if(this.loadError&&!this.detail)return w(e(`d11f19a`),this.loadError,()=>this.refresh());switch(this.tab){case`overview`:return this.tabOverview();case`runtime`:case`messaging`:return this.sectionGroups(L[this.tab]);case`permissions`:return this.tabPermissions();case`chats`:return this.tabChats();case`docs`:return this.tabDocs();case`yaml`:return this.tabYaml()}}render(){let e=n`${this.header()}${this.tabs()}`,t=n`${this.conflictBanner()}${this.body()}${this.saveBar()}`;return this.tab===`messaging`&&!this.detail?.summary.load_error?this.withPreview(e,t,n`<sm-preview .ctl=${this.preview} .botName=${this.detail?.summary.display||this.alias}></sm-preview>`):n`<div class="page">${e}${t}</div>`}};customElements.define(`sm-page-agent`,R);