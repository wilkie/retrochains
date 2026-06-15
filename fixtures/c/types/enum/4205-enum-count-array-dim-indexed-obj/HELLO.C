enum day { MON, TUE, WED, THU, FRI, COUNT };
int hours[COUNT];
int main(void) {
  int i;
  int total;
  i = 0;
  while (i < COUNT) {
    hours[i] = i + 1;
    i = i + 1;
  }
  total = hours[MON] + hours[WED] + hours[FRI];
  return total;
}
