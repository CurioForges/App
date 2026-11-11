// The wizard's page set and its display order.
//
// Kept out of Setup.jsx so a plain Node test can assert the two stay consistent:
// a page that exists but is missing from PAGE_ORDER never renders, and an id in
// PAGE_ORDER with no page blanks the screen (both have happened).

// Global choices come first: weights and credentials are shared by every
// installation, the data directory belongs to this one.
export const STEPS = [
  {id: 'models', title: '模型权重目录', caption: '权重与缓存', count: 2},
  {id: 'token', title: '权重下载 Token', caption: '受限模型许可', count: 4},
  {id: 'data', title: '项目数据目录', caption: '任务与数据库', count: 2},
];

export const PAGE_ORDER = ['models', 'token', 'data'];

/** Pages in the order the wizard shows them. */
export function orderedSteps() {
  return PAGE_ORDER.map(id => STEPS.find(step => step.id === id)).filter(Boolean);
}
