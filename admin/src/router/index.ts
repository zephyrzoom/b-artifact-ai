import { createRouter, createWebHistory } from 'vue-router'
import { useAuthStore } from '@/stores/auth'

const routes = [
  { path: '/login', name: 'login', component: () => import('@/views/LoginView.vue'), meta: { public: true } },
  {
    path: '/',
    component: () => import('@/layouts/MainLayout.vue'),
    children: [
      { path: '', name: 'dashboard', component: () => import('@/views/DashboardView.vue') },
      { path: 'repos', name: 'repos', component: () => import('@/views/ReposView.vue') },
      {
        path: 'repos/:name',
        name: 'repo-detail',
        component: () => import('@/views/RepoDetailView.vue'),
        props: true,
      },
      { path: 'users', name: 'users', component: () => import('@/views/UsersView.vue') },
      { path: 'groups', name: 'groups', component: () => import('@/views/GroupsView.vue') },
      { path: 'audit', name: 'audit', component: () => import('@/views/AuditView.vue') },
      { path: 'settings', name: 'settings', component: () => import('@/views/SettingsView.vue') },
    ],
  },
  { path: '/:pathMatch(.*)*', redirect: '/' },
]

export const router = createRouter({
  history: createWebHistory(import.meta.env.BASE_URL),
  routes,
})

router.beforeEach(async (to) => {
  const auth = useAuthStore()
  if (!auth.ready) await auth.load()
  if (!to.meta.public && !auth.user) {
    return { name: 'login', query: { redirect: to.fullPath } }
  }
  if (to.name === 'login' && auth.user) {
    return { name: 'dashboard' }
  }
  return true
})

export default router
