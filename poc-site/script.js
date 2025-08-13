// Smooth scrolling for navigation links
document.querySelectorAll('a[href^="#"]').forEach(anchor => {
    anchor.addEventListener('click', function (e) {
        e.preventDefault();
        const target = document.querySelector(this.getAttribute('href'));
        if (target) {
            const offsetTop = target.offsetTop - 80; // Account for fixed navbar
            window.scrollTo({
                top: offsetTop,
                behavior: 'smooth'
            });
        }
    });
});

// Form submission handling
document.querySelector('.email-signup').addEventListener('submit', function(e) {
    e.preventDefault();
    const email = this.querySelector('input[type="email"]').value;
    
    // Simple validation
    if (email && isValidEmail(email)) {
        // Show success message
        showMessage('Thanks! We\'ll send you updates about Companion.', 'success');
        this.querySelector('input[type="email"]').value = '';
    } else {
        showMessage('Please enter a valid email address.', 'error');
    }
});

function isValidEmail(email) {
    const emailRegex = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
    return emailRegex.test(email);
}

function showMessage(text, type) {
    // Remove existing messages
    const existingMessage = document.querySelector('.form-message');
    if (existingMessage) {
        existingMessage.remove();
    }
    
    // Create new message
    const message = document.createElement('div');
    message.className = `form-message ${type}`;
    message.textContent = text;
    message.style.cssText = `
        padding: 1rem;
        margin-top: 1rem;
        border-radius: 8px;
        text-align: center;
        ${type === 'success' ? 
            'background: #dcfce7; color: #059669; border: 1px solid #bbf7d0;' : 
            'background: #fef2f2; color: #dc2626; border: 1px solid #fecaca;'
        }
    `;
    
    // Insert after form
    const form = document.querySelector('.email-signup');
    form.parentNode.insertBefore(message, form.nextSibling);
    
    // Remove message after 5 seconds
    setTimeout(() => {
        message.remove();
    }, 5000);
}

// Add animation on scroll
const observerOptions = {
    threshold: 0.1,
    rootMargin: '0px 0px -50px 0px'
};

const observer = new IntersectionObserver((entries) => {
    entries.forEach(entry => {
        if (entry.isIntersecting) {
            entry.target.style.opacity = '1';
            entry.target.style.transform = 'translateY(0)';
        }
    });
}, observerOptions);

// Observe elements for animation
document.addEventListener('DOMContentLoaded', () => {
    const animatedElements = document.querySelectorAll('.problem-card, .step, .example-card, .safety-card');
    
    animatedElements.forEach(el => {
        el.style.opacity = '0';
        el.style.transform = 'translateY(30px)';
        el.style.transition = 'opacity 0.6s ease, transform 0.6s ease';
        observer.observe(el);
    });
});

// Mobile menu toggle (if you want to add a mobile menu later)
function toggleMobileMenu() {
    const navLinks = document.querySelector('.nav-links');
    navLinks.classList.toggle('mobile-active');
}

// Add floating effect to hero phone
document.addEventListener('DOMContentLoaded', () => {
    const phone = document.querySelector('.phone-mockup');
    if (phone) {
        let position = 0;
        setInterval(() => {
            position += 0.01;
            phone.style.transform = `translateY(${Math.sin(position) * 10}px)`;
        }, 50);
    }
});

// Add typing effect to chat messages
document.addEventListener('DOMContentLoaded', () => {
    const chatMessages = document.querySelectorAll('.companion-chat .message');
    
    chatMessages.forEach((message, index) => {
        message.style.opacity = '0';
        message.style.transform = 'translateY(20px)';
        
        setTimeout(() => {
            message.style.transition = 'opacity 0.5s ease, transform 0.5s ease';
            message.style.opacity = '1';
            message.style.transform = 'translateY(0)';
        }, index * 1000 + 2000); // Stagger the animations
    });
});

// Add parallax effect to sections
window.addEventListener('scroll', () => {
    const scrolled = window.pageYOffset;
    const sections = document.querySelectorAll('.hero, .problem-section, .solution-section');
    
    sections.forEach((section, index) => {
        const rate = scrolled * -0.5;
        if (index % 2 === 0) {
            section.style.transform = `translateY(${rate * 0.1}px)`;
        }
    });
});

// Add hover effects to cards
document.addEventListener('DOMContentLoaded', () => {
    const cards = document.querySelectorAll('.example-card, .safety-card, .problem-card');
    
    cards.forEach(card => {
        card.addEventListener('mouseenter', () => {
            card.style.transition = 'all 0.3s ease';
        });
    });
});