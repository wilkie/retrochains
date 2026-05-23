int h1(int x);
int h2(int x);
int h3(int x);

int (*handlers[3])(int) = {h1, h2, h3};

int dispatch(int i, int x) {
  return handlers[i](x);
}
