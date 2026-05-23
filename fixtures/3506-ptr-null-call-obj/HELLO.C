void handle(int *p);

void safe(int *p) {
  if (p != 0) handle(p);
}
