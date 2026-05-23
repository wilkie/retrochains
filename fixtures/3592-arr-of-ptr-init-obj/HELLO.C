int a = 10;
int b = 20;
int c = 30;

int *table[3] = {&a, &b, &c};

int pick(int i) {
  return *(table[i]);
}
