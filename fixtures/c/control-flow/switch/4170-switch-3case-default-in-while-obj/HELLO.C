int run(int n){int s; s=0; while(n>0){ switch(n){case 1: s=s+10; break; case 2: s=s+20; break; case 3: s=s+30; break; default: s=s+1; break;} n=n-1;} return s;}
