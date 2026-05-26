int a(int);
int b(int);

int (*tab[2])(int) = {a, b};

int run(int sel, int v) {
  return tab[sel](v);
}
