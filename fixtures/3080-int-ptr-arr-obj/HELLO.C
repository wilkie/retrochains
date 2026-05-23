int a;
int b;
int c;
int *table[3] = { &a, &b, &c };
int *get(int i) {
  return table[i];
}
