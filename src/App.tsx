import { useCallback, useEffect, useRef, useState } from 'react';
import { isTauri } from '@tauri-apps/api/core';
import { createProject } from './cache';
import { useProjectCache } from './useProjectCache';
import { Icon } from './Icon';
import { Editor } from './Editor';
import { Settings } from './SpeechSettings';
import { type Project, shortTime, sequenceLength } from './types';
import './App.css';

export function Brand({ compact = false }: { compact?: boolean }) {
  return (
    <div className="brand">
      <span className="brand-mark">
        <i />
        <i />
        <i />
      </span>
      {!compact && (
        <span>
          cutline<span className="brand-dot">.</span>
        </span>
      )}
    </div>
  );
}

function Modal({
  title,
  onClose,
  children,
  className = '',
}: {
  title: string;
  onClose: () => void;
  children: React.ReactNode;
  className?: string;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    const keydown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
      if (event.key === 'Tab') {
        const elements = ref.current?.querySelectorAll<HTMLElement>(
          'button, input, textarea, a[href], select',
        );
        if (!elements?.length) return;
        const first = elements[0];
        const last = elements[elements.length - 1];
        if (event.shiftKey && document.activeElement === first) {
          event.preventDefault();
          last.focus();
        } else if (!event.shiftKey && document.activeElement === last) {
          event.preventDefault();
          first.focus();
        }
      }
    };
    document.addEventListener('keydown', keydown);
    return () => {
      document.removeEventListener('keydown', keydown);
      previous?.focus();
    };
  }, [onClose]);
  return (
    <div
      className="modal-backdrop"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div
        className={`modal ${className}`}
        role="dialog"
        aria-modal="true"
        aria-label={title}
        ref={ref}
      >
        <div className="modal-heading">
          <h2>{title}</h2>
          <button
            className="icon-button"
            onClick={onClose}
            aria-label="Close dialog"
          >
            <Icon name="close" />
          </button>
        </div>
        {children}
      </div>
    </div>
  );
}

function NewProject({
  onClose,
  onCreate,
}: {
  onClose: () => void;
  onCreate: (name: string, description: string) => void;
}) {
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  return (
    <Modal title="A new story" onClose={onClose}>
      <div className="modal-symbol">
        <Icon name="film" size={26} />
      </div>
      <p className="modal-intro">
        Give your project a name. The rest starts with a clip.
      </p>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          if (name.trim()) onCreate(name, description);
        }}
      >
        <label className="field">
          Project name{' '}
          <input
            autoFocus
            placeholder="e.g. A weekend away"
            maxLength={80}
            required
            value={name}
            onChange={(event) => setName(event.target.value)}
          />
        </label>
        <label className="field">
          Description <span className="optional">Optional</span>
          <textarea
            placeholder="What's the story?"
            maxLength={500}
            rows={3}
            value={description}
            onChange={(event) => setDescription(event.target.value)}
          />
        </label>
        <div className="local-note">
          <Icon name="folder" size={15} /> Saved automatically on this device
        </div>
        <div className="modal-actions">
          <button className="button secondary" type="button" onClick={onClose}>
            Cancel
          </button>
          <button
            className="button primary"
            type="submit"
            disabled={!name.trim()}
          >
            Create project <Icon name="arrow" size={16} />
          </button>
        </div>
      </form>
    </Modal>
  );
}

const shortcutRows = [
  ['A', 'Previous cut / source mark'],
  ['S', 'Next cut / source mark'],
  ['V', 'Insert source selection at playhead'],
  ['X', 'Split timeline segment at playhead'],
  ['B', 'Replace timeline selection with source range'],
  ['I', 'Mark in in the active viewer'],
  ['O', 'Mark out in the active viewer'],
  ['P', 'Clear marks in the active viewer'],
  ['Space', 'Play / pause active viewer'],
  ['←  →', 'Step one frame (Shift: ten frames)'],
  ['Ctrl / ⌘ Z', 'Undo timeline edit'],
  ['Delete / ⌫', 'Delete timeline range or selected clip'],
];
export function ShortcutDialog({ onClose }: { onClose: () => void }) {
  return (
    <Modal
      title="A little muscle memory"
      onClose={onClose}
      className="shortcut-modal"
    >
      <p className="modal-intro">Familiar keys. A smoother edit.</p>
      <div className="shortcut-table">
        {shortcutRows.map(([key, label]) => (
          <div key={key}>
            <span>{label}</span>
            <kbd>{key}</kbd>
          </div>
        ))}
      </div>
      <p className="shortcut-note">
        I / O / P apply to the active viewer. Timeline marks take priority over a
        selected clip. Replace uses the source range and shifts later clips to fit.
        Shortcuts pause while you type.
      </p>
      <a
        className="text-link"
        href="https://cdn.lwks.com/resources/Lightworks-Keyboard-Assignments.pdf"
        target="_blank"
        rel="noreferrer"
      >
        Based on the classic Lightworks layout <Icon name="arrow" size={13} />
      </a>
    </Modal>
  );
}

function relativeDate(value: number) {
  const days = Math.floor((Date.now() - value) / 86400000);
  return days === 0
    ? 'Today'
    : days === 1
      ? 'Yesterday'
      : new Date(value).toLocaleDateString(undefined, {
          month: 'short',
          day: 'numeric',
        });
}

export default function App() {
  const { projects, setProjects, cacheError, loading } = useProjectCache();
  const [activeId, setActiveId] = useState(() => {
    try {
      return decodeURIComponent(location.hash.replace('#project=', ''));
    } catch {
      return '';
    }
  });
  const [newProject, setNewProject] = useState(false);
  const [shortcuts, setShortcuts] = useState(false);
  const [providerSettings, setProviderSettings] = useState(false);
  const [settingsTab, setSettingsTab] = useState<'speech' | 'providers'>('speech');
  const [providerRevision, setProviderRevision] = useState(0);
  const closeProviderSettings = useCallback(() => {
    setProviderSettings(false);
    setProviderRevision((revision) => revision + 1);
  }, []);
  const [search, setSearch] = useState('');
  const [sort, setSort] = useState('updated');
  const [view, setView] = useState<'grid' | 'list'>('grid');
  const [section, setSection] = useState('projects');
  useEffect(() => {
    const change = () => {
      try {
        setActiveId(decodeURIComponent(location.hash.replace('#project=', '')));
      } catch {
        setActiveId('');
      }
    };
    window.addEventListener('hashchange', change);
    return () => window.removeEventListener('hashchange', change);
  }, []);
  const open = (id: string) => {
    location.hash = `project=${encodeURIComponent(id)}`;
    setActiveId(id);
  };
  const active = projects.find((project) => project.id === activeId);
  const updateProject = useCallback(
    (patch: (project: Project) => Project, metadataOnly = false) =>
      setProjects((existing) =>
        existing.map((project) => {
          if (project.id !== activeId) return project;
          const next = patch(project);
          return metadataOnly || next === project
            ? next
            : { ...next, updatedAt: Date.now() };
        }),
      ),
    [activeId],
  );
  const visible = projects
    .filter((project) =>
      `${project.name} ${project.description}`
        .toLowerCase()
        .includes(search.toLowerCase()),
    )
    .sort((a, b) =>
      sort === 'name'
        ? a.name.localeCompare(b.name)
        : sort === 'created'
          ? b.createdAt - a.createdAt
          : b.updatedAt - a.updatedAt,
    );
  const recent = [...projects].sort((a, b) => b.updatedAt - a.updatedAt)[0];

  if (loading) return <main className="media-empty" role="status">Loading projects…</main>;

  return (
    <>
      {active ? (
        <Editor
          key={active.id}
          project={active}
          onUpdate={updateProject}
          onHome={() => {
            location.hash = '';
            setActiveId('');
          }}
          onShortcuts={() => setShortcuts(true)}
          onSettings={(tab = 'speech') => { setSettingsTab(tab); setProviderSettings(true); }}
          providerRevision={providerRevision}
          cacheError={cacheError}
        />
      ) : (
        <div className="home-shell">
          <aside className="home-sidebar">
            <Brand />
            <div className="workspace-label">YOUR WORKSPACE</div>
            <nav aria-label="Workspace">
              <button
                className={
                  section === 'projects' ? 'nav-item active' : 'nav-item'
                }
                onClick={() => setSection('projects')}
              >
                <Icon name="folder" />
                All projects <span>{projects.length}</span>
              </button>
              <button
                className={
                  section === 'recent' ? 'nav-item active' : 'nav-item'
                }
                onClick={() => {
                  setSection('recent');
                  setSort('updated');
                }}
              >
                <Icon name="clock" />
                Recently edited
              </button>
            </nav>
            <div className="sidebar-bottom">
              {isTauri() && (
                <button className="nav-item" onClick={() => setProviderSettings(true)}>
                  <Icon name="settings" /> Settings
                </button>
              )}
              <div className="storage-note">
                <span className={`status-dot ${cacheError ? 'error' : ''}`} />
                <div>
                  Local workspace<small>Projects saved on this device</small>
                </div>
              </div>
              <button
                className="nav-item shortcut-nav"
                onClick={() => setShortcuts(true)}
              >
                <Icon name="keyboard" />
                Keyboard shortcuts <kbd>?</kbd>
              </button>
              <div className="profile">
                <span className="avatar">Y</span>
                <div>
                  Your workspace<small>Personal space</small>
                </div>
                <span className="version">v0.1</span>
              </div>
            </div>
          </aside>
          <div className="home-body">
            <header className="home-topbar">
              <div>
                <Icon name="home" size={16} />
                <span>/</span>
                <span>Projects</span>
              </div>
              <div className="topbar-note">
                <span className="status-dot" /> A little more focus. A little
                less noise.
              </div>
            </header>
            <main className="home-main">
              {cacheError && (
                <div className="error-banner" role="alert">
                  {cacheError}
                </div>
              )}
              <div className="page-heading">
                <div>
                  <span className="eyebrow">YOUR CREATIVE SPACE</span>
                  <h1>
                    Good stories start here<span>.</span>
                  </h1>
                  <p>Pick up where you left off, or find a new beginning.</p>
                </div>
                <button
                  className="button primary"
                  onClick={() => setNewProject(true)}
                >
                  <Icon name="plus" size={17} /> New project
                </button>
              </div>
              {recent && (
                <section
                  className="recent-section"
                  aria-label="Continue editing"
                >
                  <div className="section-label">
                    <span>BACK TO THE STORY</span>
                    <span>
                      <Icon name="clock" size={13} /> Last edited{' '}
                      {relativeDate(recent.updatedAt).toLowerCase()}
                    </span>
                  </div>
                  <button
                    className="featured-project"
                    onClick={() => open(recent.id)}
                    style={
                      recent.clips.find((c) => c.thumbnail)?.thumbnail
                        ? {
                            backgroundImage: `url(${recent.clips.find((c) => c.thumbnail)?.thumbnail})`,
                          }
                        : undefined
                    }
                  >
                    <div className="featured-shade" />
                    <span className="featured-tag">
                      <span className="status-dot" />
                      {recent.demo ? 'SAMPLE PROJECT' : 'IN PROGRESS'}
                    </span>
                    <div className="featured-content">
                      <div>
                        <span className="featured-kicker">
                          A story in the making
                        </span>
                        <h2>{recent.name}</h2>
                        <p>
                          {recent.description ||
                            'Every frame is a fresh possibility.'}
                        </p>
                        <div className="featured-meta">
                          <span>
                            <Icon name="film" size={14} />
                            {recent.clips.length} clips
                          </span>
                          <span>
                            {shortTime(sequenceLength(recent.timeline))}{' '}
                            sequence
                          </span>
                          <span>24 fps</span>
                        </div>
                      </div>
                      <span className="continue-button">
                        Continue editing <Icon name="arrow" size={18} />
                      </span>
                    </div>
                  </button>
                </section>
              )}
              <section className="projects-section" aria-label="Projects">
                <div className="projects-toolbar">
                  <h2>
                    {section === 'recent' ? 'Recently edited' : 'All projects'}{' '}
                    <span>{projects.length}</span>
                  </h2>
                  <div className="project-controls">
                    <label className="search-box">
                      <Icon name="search" size={16} />
                      <input
                        aria-label="Search projects"
                        placeholder="Search projects..."
                        value={search}
                        onChange={(event) => setSearch(event.target.value)}
                      />
                      {search && (
                        <button
                          onClick={() => setSearch('')}
                          aria-label="Clear project search"
                        >
                          <Icon name="close" size={14} />
                        </button>
                      )}
                    </label>
                    <label className="sort-select">
                      <Icon name="sort" size={15} />
                      <select
                        aria-label="Sort projects"
                        value={sort}
                        onChange={(event) => setSort(event.target.value)}
                      >
                        <option value="updated">Last edited</option>
                        <option value="created">Date created</option>
                        <option value="name">Name A–Z</option>
                      </select>
                    </label>
                    <div className="view-switch">
                      <button
                        className={view === 'grid' ? 'selected' : ''}
                        aria-label="Grid view"
                        aria-pressed={view === 'grid'}
                        onClick={() => setView('grid')}
                      >
                        <Icon name="grid" size={15} />
                      </button>
                      <button
                        className={view === 'list' ? 'selected' : ''}
                        aria-label="List view"
                        aria-pressed={view === 'list'}
                        onClick={() => setView('list')}
                      >
                        <Icon name="list" size={17} />
                      </button>
                    </div>
                  </div>
                </div>
                <div
                  className={`project-grid ${view === 'list' ? 'project-list' : ''}`}
                >
                  {visible.map((project) => (
                    <button
                      className="project-card"
                      key={project.id}
                      onClick={() => open(project.id)}
                    >
                      <div className="project-cover">
                        {project.clips.find((c) => c.thumbnail)?.thumbnail ? (
                          <img
                            src={
                              project.clips.find((c) => c.thumbnail)?.thumbnail
                            }
                            alt=""
                          />
                        ) : (
                          <div className="cover-placeholder">
                            <Icon name="film" size={32} />
                            <span>Ready for your first frame</span>
                          </div>
                        )}
                        {project.demo && (
                          <span className="sample-badge">Sample project</span>
                        )}
                        <span className="project-duration">
                          {shortTime(sequenceLength(project.timeline))}
                        </span>
                        <span className="card-play">
                          <Icon name="play" size={20} />
                        </span>
                      </div>
                      <div className="project-card-info">
                        <div>
                          <h3>{project.name}</h3>
                          <p>
                            {project.clips.length} clips <span>·</span> Edited{' '}
                            {relativeDate(project.updatedAt).toLowerCase()}
                          </p>
                        </div>
                        <Icon name="arrow" size={18} />
                      </div>
                    </button>
                  ))}
                  {!search && (
                    <button
                      className="new-project-card"
                      onClick={() => setNewProject(true)}
                    >
                      <span className="new-project-icon">
                        <Icon name="plus" size={24} />
                      </span>
                      <h3>A fresh canvas</h3>
                      <p>Turn an idea into a new project</p>
                      <span>
                        Create project <Icon name="arrow" size={14} />
                      </span>
                    </button>
                  )}
                </div>
                {search && !visible.length && (
                  <div className="no-results">
                    <Icon name="search" size={30} />
                    <h3>No projects found</h3>
                    <p>Try a different name or description.</p>
                  </div>
                )}
              </section>
              <footer className="home-footer">
                <span>
                  <Icon name="check" size={14} />{' '}
                  {cacheError
                    ? 'Check local storage'
                    : 'Projects save automatically as you work'}
                </span>
                <span>Made for the way you think.</span>
              </footer>
            </main>
          </div>
        </div>
      )}
      {newProject && (
        <NewProject
          onClose={() => setNewProject(false)}
          onCreate={(name, description) => {
            const project = createProject(name, description);
            setProjects((items) => [project, ...items]);
            setNewProject(false);
            open(project.id);
          }}
        />
      )}
      {shortcuts && <ShortcutDialog onClose={() => setShortcuts(false)} />}
      {providerSettings && (
        <Modal title="Settings" onClose={closeProviderSettings} className="provider-modal">
          <Settings initialTab={settingsTab} />
        </Modal>
      )}
    </>
  );
}
