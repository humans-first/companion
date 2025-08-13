# Companion Website

A static website showcasing Companion - your AI growth partner that works backwards from your goals.

## 🚀 Live Demo

Visit the live website: [companion-website.vercel.app](https://companion-website.vercel.app)

## 🛠️ Development

This is a static website built with vanilla HTML, CSS, and JavaScript.

### Local Development

```bash
# Serve locally on port 3000
npm run dev

# Or use any static server
python3 -m http.server 3000
```

### Project Structure

```
poc-site/
├── index.html          # Main website
├── styles.css          # Styling and animations
├── script.js           # Interactive functionality
├── vercel.json         # Vercel deployment config
└── package.json        # Project metadata
```

## 🚀 Deployment

### Automatic Deployment

This site is automatically deployed to Vercel via GitHub Actions:

- **Production**: Deploys on push to `main` branch
- **Preview**: Deploys on pull requests
- **Trigger**: Only when files in `poc-site/` change

### Manual Deployment

```bash
# Install Vercel CLI
npm install -g vercel

# Deploy from poc-site folder
cd poc-site
vercel --prod
```

## 🔧 GitHub Actions Setup

The repository uses GitHub Actions for automated deployments. Required secrets:

- `VERCEL_TOKEN`: Your Vercel API token
- `VERCEL_ORG_ID`: Your Vercel organization ID  
- `VERCEL_PROJECT_ID`: Your Vercel project ID

See [setup instructions](../docs/deployment-setup.md) for details.

## 📱 Features

- **Responsive Design**: Mobile-first responsive layout
- **Smooth Animations**: Scroll-triggered animations and effects
- **Interactive Elements**: Hover effects and smooth scrolling
- **Email Signup**: Early access signup form with validation
- **SEO Optimized**: Meta tags and semantic HTML
- **Fast Loading**: Optimized assets and minimal dependencies

## 🎨 Design System

- **Primary Color**: #6366f1 (Indigo)
- **Font**: Inter (Google Fonts)
- **Animations**: CSS transitions and transforms
- **Mobile Breakpoints**: 768px, 480px

## 📄 Content Sections

1. **Hero**: Emotional hook with founder story
2. **Problem**: Current AI frustrations
3. **Solution**: How Companion works (4 steps)
4. **Examples**: Real-world use cases
5. **Safety**: Trust and security messaging
6. **CTA**: Early access signup

## 🔒 Security Headers

The site includes security headers via `vercel.json`:
- X-Content-Type-Options: nosniff
- X-Frame-Options: DENY
- X-XSS-Protection: 1; mode=block