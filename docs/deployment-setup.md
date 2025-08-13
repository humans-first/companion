# Deployment Setup Guide

This guide walks you through setting up automated deployments to Vercel using GitHub Actions.

## Prerequisites

- Vercel account (free)
- GitHub repository with the Companion website
- Admin access to the GitHub repository

## Step 1: Get Vercel Credentials

### 1.1 Generate Vercel Token

1. Go to [Vercel Dashboard](https://vercel.com/dashboard)
2. Click your profile (bottom left) → **Settings**
3. Go to **Tokens** tab
4. Click **Create Token**
5. Name: `GitHub Actions - Companion`
6. Scope: Select your account/team
7. Expiration: No expiration (or 1 year)
8. Click **Create Token**
9. **Copy the token** (you'll need it for GitHub secrets)

### 1.2 Get Project Information

```bash
# Install Vercel CLI if not already installed
npm install -g vercel

# Navigate to your poc-site folder
cd poc-site

# Link to your Vercel project (if not already linked)
vercel link

# Get your project info
vercel project ls
```

This will show you:
- **Project ID**: Something like `prj_abc123...`
- **Org ID**: Your organization/account ID

Alternatively, you can find these in your Vercel dashboard:
1. Go to your project in Vercel
2. Click **Settings** tab
3. **General** section shows Project ID
4. **Team** section shows Org ID

## Step 2: Configure GitHub Secrets

1. Go to your GitHub repository: `github.com/humans-first/companion`
2. Click **Settings** tab
3. In the left sidebar, click **Secrets and variables** → **Actions**
4. Click **New repository secret** for each of these:

### Required Secrets:

**VERCEL_TOKEN**
- Value: The token you generated in Step 1.1

**VERCEL_ORG_ID**
- Value: Your organization ID from Step 1.2

**VERCEL_PROJECT_ID**
- Value: Your project ID from Step 1.2

## Step 3: Verify Workflow

The GitHub Actions workflow is already configured in `.github/workflows/deploy.yml`.

### Workflow Features:

- **Triggers**: 
  - Push to `main` branch (production deploy)
  - Pull requests (preview deploy)
  - Only runs when `poc-site/` files change

- **Actions**:
  - Checkout code
  - Setup Node.js
  - Install Vercel CLI
  - Build and deploy to Vercel

## Step 4: Test the Setup

### 4.1 Make a Test Change

```bash
# Make a small change to test deployment
echo "<!-- Test deployment $(date) -->" >> poc-site/index.html

# Commit and push
git add poc-site/
git commit -m "test: verify GitHub Actions deployment"
git push origin main
```

### 4.2 Monitor the Deployment

1. Go to your GitHub repository
2. Click **Actions** tab
3. You should see a workflow running: "Deploy to Vercel"
4. Click on it to see the progress
5. Once complete, check your Vercel dashboard for the new deployment

## Step 5: Verify Production Deployment

1. Go to [Vercel Dashboard](https://vercel.com/dashboard)
2. Click on your Companion project
3. You should see a new deployment marked as "Production"
4. Click **Visit** to see your live site

## Troubleshooting

### Common Issues:

**"Error: No existing credentials found"**
- Make sure your `VERCEL_TOKEN` secret is correct
- Verify the token hasn't expired

**"Error: Project not found"**
- Check your `VERCEL_PROJECT_ID` is correct
- Ensure the project exists in your Vercel account

**"Error: Invalid team"**
- Verify your `VERCEL_ORG_ID` is correct
- Make sure you have access to the organization

**Workflow not triggering**
- Make sure you're pushing to the `main` branch
- Verify your changes are in the `poc-site/` folder
- Check the workflow file is in `.github/workflows/deploy.yml`

### Debug Commands:

```bash
# Check Vercel CLI connection
vercel whoami

# Verify project linking
vercel project ls

# Test local deployment
vercel --prod
```

## Step 6: Optional Enhancements

### 6.1 Custom Domain

1. In Vercel Dashboard → Project Settings → Domains
2. Add your custom domain
3. Follow DNS configuration instructions

### 6.2 Environment Variables

If you need environment variables:
1. Vercel Dashboard → Project Settings → Environment Variables
2. Add variables for production/preview
3. Reference in your workflow if needed

### 6.3 Build Optimization

For larger projects, you might want to add:
- Build caching
- Dependency installation
- Asset optimization

## Security Notes

- **Never commit tokens to your repository**
- Use GitHub Secrets for all sensitive data
- Regularly rotate your Vercel tokens
- Review deployment logs for any exposed information

## Next Steps

Once your automated deployment is working:

1. **Create a staging branch** for testing changes
2. **Set up branch protection rules** on main
3. **Configure notifications** for deployment status
4. **Monitor performance** with Vercel Analytics

Your Companion website will now automatically deploy whenever you push changes to the `poc-site` folder! 🚀