int a[6] = { 4, 9, 2, 8, 5, 1 };

int main(void) {
  int *p;
  int *best;
  best = a;
  for (p = a + 1; p < a + 6; p++) {
    if (*p > *best) {
      best = p;
    }
  }
  return (int)(best - a);
}
