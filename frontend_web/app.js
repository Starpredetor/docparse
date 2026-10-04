const apiBase = window.location.origin;

const queryDefaults = {
  top_k: 5,
  temperature: 0.7,
  top_p: 0.9,
  generation_top_k: 40,
  max_tokens: 700,
  frequency_penalty: 0.2,
  presence_penalty: 0.15,
  query_type: "auto",
  output_mode: "auto",
  stream_chunk_chars: 120,
};

const state = {
  currentProject: "default",
  projects: [],
  documents: [],
  chats: [],
  activeTab: "chat",
  isSending: false,
  graph: { nodes: [], edges: [] },
};

function byId(id) {
  return document.getElementById(id);
}

function escapeHtml(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
}

function inlineMarkdown(line) {
  return escapeHtml(line)
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\*([^*]+)\*/g, "<em>$1</em>");
}

function renderMarkdownTextBlock(text) {
  const lines = String(text || "").replace(/\r\n/g, "\n").split("\n");
  const html = [];
  let paragraph = [];
  let list = [];

  function flushParagraph() {
    if (!paragraph.length) return;
    html.push(`<p>${paragraph.map(inlineMarkdown).join("<br>")}</p>`);
    paragraph = [];
  }

  function flushList() {
    if (!list.length) return;
    html.push(`<ul>${list.map((item) => `<li>${inlineMarkdown(item)}</li>`).join("")}</ul>`);
    list = [];
  }

  lines.forEach((line) => {
    const trimmed = line.trim();
    if (!trimmed) {
      flushParagraph();
      flushList();
      return;
    }

    const bullet = trimmed.match(/^[-*]\s+(.+)$/);
    if (bullet) {
      flushParagraph();
      list.push(bullet[1]);
      return;
    }

    flushList();
    paragraph.push(trimmed);
  });

  flushParagraph();
  flushList();
  return html.join("");
}

function renderMarkdown(text) {
  const raw = String(text || "");
  const blocks = [];
  let cursor = 0;
  const fencePattern = /```[\w-]*\n([\s\S]*?)```/g;
  let match;

  while ((match = fencePattern.exec(raw))) {
    blocks.push({ type: "text", value: raw.slice(cursor, match.index) });
    blocks.push({ type: "code", value: match[1] });
    cursor = match.index + match[0].length;
  }
  blocks.push({ type: "text", value: raw.slice(cursor) });

  return blocks
    .map((block) => block.type === "code" ? `<pre><code>${escapeHtml(block.value)}</code></pre>` : renderMarkdownTextBlock(block.value))
    .join("");
}

async function fetchJson(url, options = {}) {
  const response = await fetch(url, options);
  const text = await response.text();
  let payload = {};
  try {
    payload = text ? JSON.parse(text) : {};
  } catch {
    payload = { message: text };
  }

  if (!response.ok) {
    const detail = payload.detail || payload.message || response.statusText;
    throw new Error(typeof detail === "string" ? detail : JSON.stringify(detail));
  }
  return payload;
}

function setStatus(text, variant = "") {
  const pill = byId("status-pill");
  pill.textContent = text;
  pill.className = `status-pill ${variant}`.trim();
}

let toastTimer;
function showToast(message) {
  const toast = byId("toast");
  toast.textContent = message;
  toast.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => toast.classList.remove("show"), 3200);
}

function formatDate(seconds) {
  if (!seconds) return "Recently";
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(new Date(seconds * 1000));
}

function hashFor(projectId) {
  return `#/workspace/${encodeURIComponent(projectId || state.currentProject || "default")}`;
}

function parseHash() {
  const hash = window.location.hash || "#/dashboard";
  const parts = hash.replace(/^#\/?/, "").split("/").filter(Boolean).map(decodeURIComponent);
  if (parts[0] === "workspace") {
    return { view: "workspace", projectId: parts[1] || "default" };
  }
  return { view: "dashboard" };
}

function showDashboardView() {
  byId("view-dashboard").classList.add("active");
  byId("view-workspace").classList.remove("active");
  document.querySelector(".shell").classList.add("dashboard-active");
  renderDashboardCards();
}

function showWorkspaceView() {
  byId("view-dashboard").classList.remove("active");
  byId("view-workspace").classList.add("active");
  document.querySelector(".shell").classList.remove("dashboard-active");
}

function renderDashboardCards() {
  const root = byId("dashboard-notebooks-grid");
  if (!state.projects || !state.projects.length) {
    root.innerHTML = "<div class='empty-state'>No notebooks yet. Click '+ New Notebook' to create one!</div>";
    return;
  }

  root.innerHTML = "";
  state.projects.forEach((project) => {
    const card = document.createElement("div");
    card.className = "notebook-card";
    
    const docCount = project.document_count || 0;
    const chunkCount = project.chunk_count || 0;
    
    const deleteBtnHtml = project.id !== "default"
      ? `<button class="notebook-card-delete-btn" type="button" title="Delete Notebook" data-project-id="${escapeHtml(project.id)}">×</button>`
      : "";

    card.innerHTML = `
      <div class="notebook-card-content">
        <div class="notebook-card-header">
          <h3 class="notebook-card-title">${escapeHtml(project.name)}</h3>
          ${deleteBtnHtml}
        </div>
        <span class="notebook-card-date">Created ${formatDate(project.created_at)}</span>
      </div>
      <div class="notebook-card-stats">
        <div class="notebook-card-stat">
          <span class="label">Sources</span>
          <span class="val">${docCount}</span>
        </div>
        <div class="notebook-card-stat">
          <span class="label">Chunks</span>
          <span class="val">${chunkCount}</span>
        </div>
      </div>
    `;
    
    // Clicking card opens the project
    card.addEventListener("click", () => {
      openProject(project.id);
    });
    
    // Handle delete click inside card
    if (project.id !== "default") {
      card.querySelector(".notebook-card-delete-btn").addEventListener("click", async (e) => {
        e.stopPropagation(); // prevent card click opening
        if (!window.confirm(`Delete notebook "${project.name}" and all its documents?`)) return;
        
        try {
          await fetchJson(`${apiBase}/workspace/projects/${encodeURIComponent(project.id)}`, {
            method: "DELETE"
          });
          showToast(`Notebook "${project.name}" deleted.`);
          
          if (state.currentProject === project.id) {
            state.currentProject = "default";
          }
          
          await refreshDashboard();
          if (window.location.hash === "#/dashboard") {
            showDashboardView();
          } else {
            window.location.hash = "#/dashboard";
          }
        } catch (err) {
          showToast(`Delete failed: ${err.message}`);
        }
      });
    }
    
    root.appendChild(card);
  });
}

function switchTab(tabName) {
  state.activeTab = tabName;
  
  // Update Tab buttons active state
  document.querySelectorAll(".tab-button").forEach((btn) => {
    const isActive = btn.dataset.tab === tabName;
    btn.classList.toggle("active", isActive);
    btn.setAttribute("aria-selected", isActive ? "true" : "false");
  });

  // Update panels active state
  document.querySelectorAll(".workspace-panel").forEach((panel) => {
    panel.classList.toggle("active", panel.id === `panel-${tabName}`);
  });

  // Load Graph if switched to graph tab
  if (tabName === "graph") {
    loadGraph();
  }
}

function currentProject() {
  return state.projects.find((project) => project.id === state.currentProject);
}

function updateProjectLabel() {
  byId("chat-project-name").textContent = currentProject()?.name || "Default Notebook";
}

function renderProjectList(projects = []) {
  const root = byId("shell-projects");
  if (!projects.length) {
    root.innerHTML = "<div class='empty-state'>No notebooks yet.</div>";
    return;
  }

  root.innerHTML = "";
  projects.forEach((project) => {
    const item = document.createElement("div");
    item.className = `project-row ${project.id === state.currentProject ? "active" : ""}`;
    
    const deleteBtnHtml = project.id !== "default"
      ? `<button class="delete-project-btn" type="button" title="Delete Notebook" data-project-id="${escapeHtml(project.id)}">×</button>`
      : "";

    item.innerHTML = `
      <span class="project-main">
        <span class="item-title">${escapeHtml(project.name)}</span>
        <span class="item-meta">${formatDate(project.created_at)}</span>
      </span>
      ${deleteBtnHtml}
    `;
    
    // Click on main area selects the project
    item.querySelector(".project-main").addEventListener("click", () => openProject(project.id));
    
    // Add delete handler if button exists
    if (project.id !== "default") {
      item.querySelector(".delete-project-btn").addEventListener("click", async (e) => {
        e.stopPropagation(); // prevent select click
        if (!window.confirm(`Delete notebook "${project.name}" and all its documents?`)) return;
        
        try {
          await fetchJson(`${apiBase}/workspace/projects/${encodeURIComponent(project.id)}`, {
            method: "DELETE"
          });
          showToast(`Notebook "${project.name}" deleted.`);
          
          if (state.currentProject === project.id) {
            state.currentProject = "default";
          }
          
          await refreshDashboard();
          if (window.location.hash === "#/dashboard") {
            showDashboardView();
          } else {
            window.location.hash = "#/dashboard";
          }
        } catch (err) {
          showToast(`Delete failed: ${err.message}`);
        }
      });
    }

    root.appendChild(item);
  });
}

async function refreshDashboard() {
  const payload = await fetchJson(`${apiBase}/workspace/dashboard`);
  state.projects = payload.projects || [];
  state.chats = payload.recent_chats || [];

  if (!state.projects.some((project) => project.id === state.currentProject) && state.projects.length) {
    state.currentProject = state.projects[0].id;
  }

  renderProjectList(state.projects);
  updateProjectLabel();
}

async function openProject(projectId) {
  state.currentProject = projectId;
  // Update hash
  const nextHash = hashFor(projectId);
  if (window.location.hash !== nextHash) window.location.hash = nextHash;
  
  await refreshDashboard();
  showWorkspaceView();
  await loadProject();
}

async function loadProject() {
  const payload = await fetchJson(`${apiBase}/workspace/projects/${encodeURIComponent(state.currentProject)}`);
  state.documents = payload.documents || [];
  state.chats = payload.recent_chats || [];
  
  // Keep the active tab, but if we are on the graph tab we'll reload it
  renderDocuments();
  renderProjectChatHistory();
  
  if (state.activeTab === "graph") {
    loadGraph();
  }
}

async function loadProjectDocuments() {
  const payload = await fetchJson(`${apiBase}/workspace/projects/${encodeURIComponent(state.currentProject)}/documents`);
  state.documents = payload.items || [];
  renderDocuments();
}

function renderDocuments() {
  const root = byId("chat-documents");
  const query = byId("doc-search").value.trim().toLowerCase();
  const docs = query
    ? state.documents.filter((doc) => (doc.filename || "").toLowerCase().includes(query))
    : state.documents;

  // Toggle Welcome / Chat state
  const welcome = byId("notebook-welcome");
  const chatMessages = byId("chat-messages");
  const chatForm = byId("chat-form");
  
  if (!state.documents || state.documents.length === 0) {
    welcome.classList.remove("hidden");
    chatMessages.classList.add("hidden");
    chatForm.classList.add("hidden");
    byId("sources-count").textContent = "0";
  } else {
    welcome.classList.add("hidden");
    chatMessages.classList.remove("hidden");
    chatForm.classList.remove("hidden");
    byId("sources-count").textContent = state.documents.length;
  }

  if (!docs.length) {
    root.innerHTML = "<div class='empty-state'>No documents found.</div>";
    return;
  }

  root.innerHTML = "";
  docs.slice().reverse().forEach((doc) => {
    const item = document.createElement("div");
    item.className = "file-row";
    
    let statusBadge = "";
    if (doc.status === "processing") {
      statusBadge = `<span class="badge processing"><span class="badge-spinner"></span>Uploading...</span>`;
    } else if (doc.status === "success") {
      statusBadge = `<span class="badge success">${escapeHtml(doc.status)}</span>`;
    } else {
      statusBadge = `<span class="badge danger" style="background: rgba(238, 119, 112, 0.12); color: var(--danger); border-color: rgba(238, 119, 112, 0.2);">${escapeHtml(doc.status)}</span>`;
    }

    const deleteBtnHtml = String(doc.id).startsWith("temp-")
      ? ""
      : `<button class="delete-button" type="button" title="Delete document" data-doc-id="${escapeHtml(doc.id)}">×</button>`;

    item.innerHTML = `
      <span class="file-main">
        <span class="item-title">${escapeHtml(doc.filename)}</span>
        <span class="item-meta">${doc.chunks_added || 0} chunks</span>
      </span>
      <span class="file-actions">
        ${statusBadge}
        ${deleteBtnHtml}
      </span>
    `;
    root.appendChild(item);
  });
}

function createMessage(role, content) {
  const message = document.createElement("article");
  message.className = `message ${role}`;
  message.innerHTML = `
    <span class="role">${role === "user" ? "You" : "Assistant"}</span>
    <div class="message-content">${renderMarkdown(content || "")}</div>
  `;
  byId("chat-messages").appendChild(message);
  scrollChatToBottom();
  return message.querySelector(".message-content");
}

function renderProjectChatHistory() {
  const root = byId("chat-messages");
  root.innerHTML = "";

  const recent = [...state.chats].reverse();
  if (!recent.length) {
    createMessage("assistant", "Hi there! I am ready to help. Upload document sources in the sidebar, and ask me anything about them.");
    return;
  }

  recent.forEach((chat) => {
    createMessage("user", chat.query || "");
    createMessage("assistant", chat.answer || "");
  });
}

function scrollChatToBottom() {
  const root = byId("chat-messages");
  root.scrollTop = root.scrollHeight;
}

function autoSizeChatInput() {
  const input = byId("chat-input");
  input.style.height = "auto";
  input.style.height = `${Math.min(input.scrollHeight, 140)}px`;
}

async function createProject(name) {
  const payload = await fetchJson(`${apiBase}/workspace/projects`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name }),
  });
  return payload.item;
}

async function uploadFiles(projectId, files) {
  const formData = new FormData();
  formData.append("project_id", projectId);
  Array.from(files).forEach((file) => formData.append("files", file));

  const response = await fetch(`${apiBase}/upload`, { method: "POST", body: formData });
  const payload = await response.json();
  if (!response.ok) throw new Error(payload.detail || JSON.stringify(payload));
  return payload;
}

async function sendChat() {
  const input = byId("chat-input");
  const query = input.value.trim();
  if (!query || state.isSending) return;

  state.isSending = true;
  input.disabled = true;
  byId("chat-send-btn").disabled = true;
  setStatus("Thinking", "busy");

  createMessage("user", query);
  input.value = "";
  autoSizeChatInput();
  const answerNode = createMessage("assistant", "Thinking...");

  try {
    const start = await fetchJson(`${apiBase}/query/start`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ ...queryDefaults, query, project_id: state.currentProject }),
    });

    let full = "";
    let done = false;

    while (!done) {
      const next = await fetchJson(`${apiBase}/query/next/${encodeURIComponent(start.job_id)}?max_chunks=5`);
      if (next.error) throw new Error(next.error);

      full += next.delta || "";
      answerNode.innerHTML = renderMarkdown(full || "Thinking...");
      scrollChatToBottom();

      done = !!next.done;
      if (!done) {
        await new Promise((resolve) => setTimeout(resolve, 90));
      }
    }

    await refreshDashboard();
    setStatus("Ready", "ok");
  } catch (error) {
    answerNode.innerHTML = renderMarkdown(`Query failed: ${error.message}`);
    setStatus("Error", "error");
    showToast(`Query failed: ${error.message}`);
  } finally {
    state.isSending = false;
    input.disabled = false;
    byId("chat-send-btn").disabled = false;
    input.focus();
  }
}

function wireEvents() {
  // Tab clicks
  byId("tab-btn-chat").addEventListener("click", () => switchTab("chat"));
  byId("tab-btn-graph").addEventListener("click", () => switchTab("graph"));



  // Dashboard new project button
  byId("dashboard-new-project-btn").addEventListener("click", async () => {
    const name = prompt("Enter a name for the new notebook:");
    if (!name) return;
    const trimmed = name.trim();
    if (trimmed.length < 2) {
      showToast("Name must be at least 2 characters.");
      return;
    }
    setStatus("Creating", "busy");
    try {
      const project = await createProject(trimmed);
      await refreshDashboard();
      await openProject(project.id);
      setStatus("Ready", "ok");
      showToast(`Notebook "${project.name}" created!`);
    } catch (error) {
      setStatus("Error", "error");
      showToast(`Creation failed: ${error.message}`);
    }
  });

  // Instant uploader when files are selected
  byId("chat-upload-files").addEventListener("change", async () => {
    const input = byId("chat-upload-files");
    if (!input.files || !input.files.length) return;

    const filesToUpload = Array.from(input.files);
    setStatus("Uploading", "busy");

    // Inject temporary placeholders to show uploading and indexing status immediately
    const tempDocs = filesToUpload.map(file => ({
      id: `temp-${Date.now()}-${file.name}`,
      filename: file.name,
      chunks_added: 0,
      status: "processing"
    }));

    state.documents = [...tempDocs, ...state.documents];
    renderDocuments();

    try {
      const upload = await uploadFiles(state.currentProject, input.files);
      input.value = "";
      await refreshDashboard();
      await loadProject();
      setStatus("Ready", "ok");
      showToast(`Ingested and indexed ${upload.succeeded}/${upload.total_files} document sources. Graph construction running in background.`);
    } catch (error) {
      // Reload correct document state on error to prune temporary placeholders
      await loadProjectDocuments();
      setStatus("Error", "error");
      showToast(`Ingestion failed: ${error.message}`);
    }
  });

  // Source list delete button clicks
  byId("chat-documents").addEventListener("click", async (event) => {
    const docId = event.target.getAttribute("data-doc-id");
    if (!docId) return;
    if (!window.confirm("Delete this document from the notebook?")) return;

    try {
      await fetchJson(`${apiBase}/workspace/projects/${encodeURIComponent(state.currentProject)}/documents/${encodeURIComponent(docId)}`, {
        method: "DELETE",
      });
      await refreshDashboard();
      await loadProjectDocuments();
      showToast("Source deleted.");
    } catch (error) {
      showToast(`Delete failed: ${error.message}`);
    }
  });

  byId("home-link").addEventListener("click", (event) => {
    event.preventDefault();
    window.location.hash = "#/dashboard";
  });

  byId("doc-search").addEventListener("input", renderDocuments);
  byId("chat-form").addEventListener("submit", (event) => {
    event.preventDefault();
    sendChat();
  });
  byId("chat-input").addEventListener("input", autoSizeChatInput);
  byId("chat-input").addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      sendChat();
    }
  });

  // Listening for hash changes
  window.addEventListener("hashchange", async () => {
    const route = parseHash();
    if (route.view === "dashboard") {
      await refreshDashboard();
      showDashboardView();
    } else {
      if (route.projectId !== state.currentProject) {
        await openProject(route.projectId);
      } else {
        showWorkspaceView();
      }
    }
  });

  // Graph actions
  byId("refresh-graph-btn").addEventListener("click", () => loadGraph());
  byId("graph-zoom-in").addEventListener("click", () => graphSim?.zoomIn());
  byId("graph-zoom-out").addEventListener("click", () => graphSim?.zoomOut());
  byId("graph-reset").addEventListener("click", () => graphSim?.reset());

  window.addEventListener("resize", () => {
    if (state.activeTab === "graph" && graphSim) {
      const canvas = byId("graph-canvas");
      const container = canvas.parentElement;
      canvas.width = container.clientWidth;
      canvas.height = Math.max(500, container.clientHeight);
    }
  });
}

// ==========================================
// Force-Directed Graph Layout Simulation Class
// ==========================================
let graphSim = null;

class GraphSimulation {
  constructor(canvas, nodes, edges, onSelectNode, onSelectEdge) {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d");
    this.nodes = nodes.map(n => ({
      ...n,
      x: (Math.random() - 0.5) * 300 + canvas.width / 2,
      y: (Math.random() - 0.5) * 300 + canvas.height / 2,
      vx: 0,
      vy: 0,
      radius: Math.max(8, Math.min(24, 6 + (n.count || 1) * 2))
    }));
    
    this.edges = edges.map(e => {
      const srcNode = this.nodes.find(n => n.id.toLowerCase() === e.source.toLowerCase());
      const tgtNode = this.nodes.find(n => n.id.toLowerCase() === e.target.toLowerCase());
      return {
        ...e,
        sourceNode: srcNode,
        targetNode: tgtNode
      };
    }).filter(e => e.sourceNode && e.targetNode);

    this.onSelectNode = onSelectNode;
    this.onSelectEdge = onSelectEdge;

    this.zoom = 1.0;
    this.panX = 0;
    this.panY = 0;
    this.hoveredNode = null;
    this.hoveredEdge = null;
    this.selectedNode = null;
    this.selectedEdge = null;
    this.draggedNode = null;
    this.isPanning = false;
    this.startX = 0;
    this.startY = 0;

    this.running = true;
    this.setupEvents();
    this.tick();
  }

  tick() {
    if (!this.running) return;

    // 1. Coulomb repulsion between all nodes
    const k_repulsion = 240;
    for (let i = 0; i < this.nodes.length; i++) {
      const n1 = this.nodes[i];
      for (let j = i + 1; j < this.nodes.length; j++) {
        const n2 = this.nodes[j];
        const dx = n2.x - n1.x;
        const dy = n2.y - n1.y;
        const dist = Math.sqrt(dx * dx + dy * dy) || 1;
        if (dist < 400) {
          const force = (k_repulsion * k_repulsion) / dist;
          const fx = (dx / dist) * force;
          const fy = (dy / dist) * force;
          n1.vx -= fx * 0.15;
          n1.vy -= fy * 0.15;
          n2.vx += fx * 0.15;
          n2.vy += fy * 0.15;
        }
      }
    }

    // 2. Attraction pull along links (spring Hooke's Law)
    const k_attraction = 0.05;
    const rest_length = 130;
    this.edges.forEach(e => {
      const n1 = e.sourceNode;
      const n2 = e.targetNode;
      const dx = n2.x - n1.x;
      const dy = n2.y - n1.y;
      const dist = Math.sqrt(dx * dx + dy * dy) || 1;
      const force = (dist - rest_length) * k_attraction;
      const fx = (dx / dist) * force;
      const fy = (dy / dist) * force;
      n1.vx += fx;
      n1.vy += fy;
      n2.vx -= fx;
      n2.vy -= fy;
    });

    // 3. Gravity center pull, damping and positioning
    const gravity = 0.03;
    const cx = this.canvas.width / 2;
    const cy = this.canvas.height / 2;
    this.nodes.forEach(n => {
      if (n === this.draggedNode) return;

      n.vx += (cx - n.x) * gravity;
      n.vy += (cy - n.y) * gravity;

      // Friction/Damping
      n.vx *= 0.82;
      n.vy *= 0.82;

      n.x += n.vx;
      n.y += n.vy;
    });

    this.render();
    requestAnimationFrame(() => this.tick());
  }

  render() {
    const ctx = this.ctx;
    ctx.clearRect(0, 0, this.canvas.width, this.canvas.height);

    ctx.save();
    ctx.translate(this.panX, this.panY);
    ctx.scale(this.zoom, this.zoom);

    // Draw Links
    this.edges.forEach(e => {
      const n1 = e.sourceNode;
      const n2 = e.targetNode;
      const isHovered = e === this.hoveredEdge;
      const isSelected = e === this.selectedEdge;

      ctx.beginPath();
      ctx.moveTo(n1.x, n1.y);
      ctx.lineTo(n2.x, n2.y);
      ctx.lineWidth = isSelected ? 3.5 : (isHovered ? 2.5 : 1.2);
      
      if (isSelected) {
        ctx.strokeStyle = "rgba(56, 184, 146, 0.95)";
      } else if (isHovered) {
        ctx.strokeStyle = "rgba(245, 158, 11, 0.85)";
      } else {
        ctx.strokeStyle = "rgba(56, 184, 146, 0.18)";
      }
      ctx.stroke();

      // Label relationships
      if (isHovered || isSelected || this.nodes.length < 30) {
        const mx = (n1.x + n2.x) / 2;
        const my = (n1.y + n2.y) / 2;
        ctx.save();
        ctx.font = "bold 9px sans-serif";
        ctx.textAlign = "center";
        ctx.textBaseline = "middle";
        
        const text = e.relation;
        const tw = ctx.measureText(text).width + 6;
        
        ctx.fillStyle = "#090c0f";
        ctx.fillRect(mx - tw / 2, my - 6, tw, 12);
        
        ctx.fillStyle = isSelected ? "#38b892" : (isHovered ? "#f59e0b" : "#8c9ba5");
        ctx.fillText(text, mx, my);
        ctx.restore();
      }
    });

    // Draw Nodes
    this.nodes.forEach(n => {
      const isHovered = n === this.hoveredNode;
      const isSelected = n === this.selectedNode;

      ctx.beginPath();
      ctx.arc(n.x, n.y, n.radius, 0, Math.PI * 2);

      if (isSelected) {
        ctx.shadowColor = "#38b892";
        ctx.shadowBlur = 20;
        ctx.fillStyle = "#38b892";
        ctx.strokeStyle = "#ffffff";
        ctx.lineWidth = 2.5;
      } else if (isHovered) {
        ctx.shadowColor = "#f59e0b";
        ctx.shadowBlur = 15;
        ctx.fillStyle = "#f59e0b";
        ctx.strokeStyle = "rgba(255, 255, 255, 0.85)";
        ctx.lineWidth = 2;
      } else {
        ctx.shadowBlur = 0;
        ctx.fillStyle = "rgba(56, 184, 146, 0.85)";
        ctx.strokeStyle = "#090c0f";
        ctx.lineWidth = 1.5;
      }
      ctx.fill();
      ctx.stroke();

      // Node text label
      ctx.shadowBlur = 0;
      ctx.font = isSelected ? "bold 13px sans-serif" : "11px sans-serif";
      ctx.fillStyle = isSelected ? "#ffffff" : (isHovered ? "#f59e0b" : "#adbac7");
      ctx.textAlign = "center";
      ctx.fillText(n.label, n.x, n.y - n.radius - 6);
    });

    ctx.restore();
  }

  setupEvents() {
    const canvas = this.canvas;

    const getMousePos = (e) => {
      const rect = canvas.getBoundingClientRect();
      const clientX = e.touches ? e.touches[0].clientX : e.clientX;
      const clientY = e.touches ? e.touches[0].clientY : e.clientY;
      const mx = clientX - rect.left;
      const my = clientY - rect.top;
      const wx = (mx - this.panX) / this.zoom;
      const wy = (my - this.panY) / this.zoom;
      return { mx, my, wx, wy };
    };

    const findHovered = (wx, wy) => {
      // Find node under coordinates
      for (let i = this.nodes.length - 1; i >= 0; i--) {
        const n = this.nodes[i];
        const dx = n.x - wx;
        const dy = n.y - wy;
        if (dx * dx + dy * dy <= (n.radius + 6) * (n.radius + 6)) {
          return { node: n, edge: null };
        }
      }

      // Check if mouse is hovering over an edge
      let foundEdge = null;
      let minDistance = 10;
      this.edges.forEach(e => {
        const n1 = e.sourceNode;
        const n2 = e.targetNode;
        const A = wx - n1.x;
        const B = wy - n1.y;
        const C = n2.x - n1.x;
        const D = n2.y - n1.y;
        const dot = A * C + B * D;
        const lenSq = C * C + D * D;
        let param = -1;
        if (lenSq !== 0) param = dot / lenSq;

        let xx, yy;
        if (param < 0) {
          xx = n1.x;
          yy = n1.y;
        } else if (param > 1) {
          xx = n2.x;
          yy = n2.y;
        } else {
          xx = n1.x + param * C;
          yy = n1.y + param * D;
        }

        const dx = wx - xx;
        const dy = wy - yy;
        const dist = Math.sqrt(dx * dx + dy * dy);
        if (dist < minDistance) {
          minDistance = dist;
          foundEdge = e;
        }
      });

      return { node: null, edge: foundEdge };
    };

    const onStart = (e) => {
      const pos = getMousePos(e);
      const h = findHovered(pos.wx, pos.wy);
      
      if (h.node) {
        this.draggedNode = h.node;
        this.selectedNode = h.node;
        this.selectedEdge = null;
        this.onSelectNode(h.node);
      } else if (h.edge) {
        this.selectedEdge = h.edge;
        this.selectedNode = null;
        this.onSelectEdge(h.edge);
      } else {
        this.isPanning = true;
        this.startX = pos.mx - this.panX;
        this.startY = pos.my - this.panY;
      }
    };

    const onMove = (e) => {
      const pos = getMousePos(e);

      if (this.draggedNode) {
        this.draggedNode.x = pos.wx;
        this.draggedNode.y = pos.wy;
        this.draggedNode.vx = 0;
        this.draggedNode.vy = 0;
      } else if (this.isPanning) {
        this.panX = pos.mx - this.startX;
        this.panY = pos.my - this.startY;
      } else {
        const h = findHovered(pos.wx, pos.wy);
        this.hoveredNode = h.node;
        this.hoveredEdge = h.edge;
      }
    };

    const onEnd = () => {
      this.draggedNode = null;
      this.isPanning = false;
    };

    canvas.addEventListener("mousedown", onStart);
    canvas.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onEnd);

    canvas.addEventListener("touchstart", onStart, { passive: true });
    canvas.addEventListener("touchmove", onMove, { passive: true });
    window.addEventListener("touchend", onEnd);

    canvas.addEventListener("wheel", (e) => {
      e.preventDefault();
      const pos = getMousePos(e);
      const zoomFactor = e.deltaY < 0 ? 1.15 : 0.85;
      const newZoom = Math.max(0.15, Math.min(4.0, this.zoom * zoomFactor));

      this.panX = pos.mx - (pos.mx - this.panX) * (newZoom / this.zoom);
      this.panY = pos.my - (pos.my - this.panY) * (newZoom / this.zoom);
      this.zoom = newZoom;
    }, { passive: false });
  }

  zoomIn() {
    this.zoom = Math.min(4.0, this.zoom * 1.3);
  }

  zoomOut() {
    this.zoom = Math.max(0.15, this.zoom * 0.77);
  }

  reset() {
    this.zoom = 1.0;
    this.panX = 0;
    this.panY = 0;
    this.selectedNode = null;
    this.selectedEdge = null;
    this.hoveredNode = null;
    this.hoveredEdge = null;
    
    const cx = this.canvas.width / 2;
    const cy = this.canvas.height / 2;
    this.nodes.forEach(n => {
      n.x = (Math.random() - 0.5) * 200 + cx;
      n.y = (Math.random() - 0.5) * 200 + cy;
      n.vx = 0;
      n.vy = 0;
    });
  }

  destroy() {
    this.running = false;
  }
}

// ==========================================
// Knowledge Graph Fetching and Inspector Builders
// ==========================================
async function loadGraph() {
  const loading = byId("graph-loading");
  const empty = byId("graph-empty");
  const canvas = byId("graph-canvas");
  const inspector = byId("inspector-content");
  
  loading.classList.remove("hidden");
  empty.classList.add("hidden");
  inspector.innerHTML = `<div class="empty-state">Click on any entity node or connection line to inspect details.</div>`;
  
  if (graphSim) {
    graphSim.destroy();
    graphSim = null;
  }

  try {
    const payload = await fetchJson(`${apiBase}/workspace/projects/${encodeURIComponent(state.currentProject)}/graph`);
    state.graph = payload || { nodes: [], edges: [] };
    
    const container = canvas.parentElement;
    canvas.width = container.clientWidth;
    canvas.height = Math.max(500, container.clientHeight);

    if (!state.graph.nodes || !state.graph.nodes.length) {
      empty.classList.remove("hidden");
      const ctx = canvas.getContext("2d");
      ctx.clearRect(0, 0, canvas.width, canvas.height);
      return;
    }

    graphSim = new GraphSimulation(
      canvas,
      state.graph.nodes,
      state.graph.edges,
      (node) => selectInspectorNode(node),
      (edge) => selectInspectorEdge(edge)
    );
  } catch (error) {
    showToast(`Failed to load graph: ${error.message}`);
  } finally {
    loading.classList.add("hidden");
  }
}

function selectInspectorNode(node) {
  const root = byId("inspector-content");
  
  const connections = state.graph.edges.filter(
    e => e.source.toLowerCase() === node.id.toLowerCase() || e.target.toLowerCase() === node.id.toLowerCase()
  );

  const relationsHtml = connections.map(e => {
    const isSource = e.source.toLowerCase() === node.id.toLowerCase();
    const otherNode = isSource ? e.target : e.source;
    const direction = isSource ? `&rarr;` : `&larr;`;
    return `
      <div class="inspector-relation-item">
        ${isSource ? `<strong>This Node</strong>` : escapeHtml(e.source)}
        <span class="rel-type">${escapeHtml(e.relation)}</span>
        ${isSource ? escapeHtml(e.target) : `<strong>This Node</strong>`}
      </div>
    `;
  }).join("");

  root.innerHTML = `
    <div class="inspector-card">
      <span class="inspector-type">${escapeHtml(node.type || "Entity")}</span>
      <h3>${escapeHtml(node.id)}</h3>
      
      <div class="inspector-metric" style="margin-top: 12px;">
        <span>Document Frequency</span>
        <strong>${node.count || 1}</strong>
      </div>
      
      <div class="inspector-relations-title">Direct Connections (${connections.length})</div>
      <div class="inspector-relations-list">
        ${relationsHtml || `<div class="empty-state">No direct connections.</div>`}
      </div>
    </div>
  `;
}

function selectInspectorEdge(edge) {
  const root = byId("inspector-content");
  root.innerHTML = `
    <div class="inspector-card">
      <span class="inspector-type" style="background: rgba(245, 158, 11, 0.15); color: var(--warning);">Relationship</span>
      <h3>${escapeHtml(edge.relation)}</h3>
      
      <div class="inspector-metric" style="margin-top: 12px;">
        <span>Source Entity</span>
        <strong>${escapeHtml(edge.source)}</strong>
      </div>
      
      <div class="inspector-metric">
        <span>Target Entity</span>
        <strong>${escapeHtml(edge.target)}</strong>
      </div>
      
      <div class="inspector-metric" style="margin-top: 12px; padding-top: 12px; border-top: 1px solid var(--line);">
        <span>Extraction Weight</span>
        <strong>${edge.count || 1}</strong>
      </div>
    </div>
  `;
}

async function init() {
  wireEvents();

  try {
    const route = parseHash();
    if (route.view === "dashboard") {
      await refreshDashboard();
      showDashboardView();
    } else {
      state.currentProject = route.projectId;
      await refreshDashboard();
      await openProject(route.projectId);
    }
    
    // Explicitly enforce Chat tab active by default
    switchTab("chat");
    
    setStatus("Ready", "ok");
  } catch (error) {
    setStatus("Offline", "error");
    showToast(`Backend unavailable: ${error.message}`);
  }
}

init();
